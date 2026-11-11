use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use moka::sync::Cache;
use object_store::path::Path;
use object_store::{ObjectStore, WriteMultipart};
use strata_git::{CommitGraph, Object, ObjectCache, ObjectKind, PackBitmaps, PackData, PackFile, PackIndex, PackSet};
use tokio::runtime::Handle;
use tokio_stream::StreamExt;

/// Sizes of the per-node caches. Both are disposable: any node can serve any
/// repository by reading from object storage again.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Granularity of ranged reads from object storage.
    pub block_size: usize,
    /// Bytes of raw pack blocks to keep.
    pub block_cache_bytes: u64,
    /// Bytes of reconstructed commits, trees and small blobs to keep.
    pub object_cache_bytes: u64,
    /// Bytes of pack indexes, commit-graphs and bitmaps to keep loaded.
    pub index_cache_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig { block_size: 4 << 20, block_cache_bytes: 1 << 30, object_cache_bytes: 256 << 20, index_cache_bytes: 2 << 30 }
    }
}

/// Blobs above this size are not kept in the object cache: they are rarely
/// read twice and would push out the trees that graph walks depend on.
const MAX_CACHED_BLOB: usize = 64 << 10;

/// Parts of one upload in flight at a time.
const UPLOAD_CONCURRENCY: usize = 4;

struct Blocks {
    store: Arc<dyn ObjectStore>,
    runtime: Handle,
    /// Keyed by (pack, block number).
    cache: Cache<(u64, u64), Bytes>,
    block_size: u64,
}

/// A pack in object storage, read one block at a time through the cache.
struct RemotePack {
    blocks: Arc<Blocks>,
    path: Path,
    size: u64,
    /// Process-unique, for cache keys.
    id: u64,
}

impl PackData for RemotePack {
    fn size(&self) -> u64 {
        self.size
    }

    /// Blocks on object storage on a cache miss, so this must run on a
    /// blocking thread, never on the async executor.
    fn chunk(&self, offset: u64) -> strata_git::Result<Bytes> {
        if offset >= self.size {
            return Ok(Bytes::new());
        }
        let blocks = &self.blocks;
        let block = offset / blocks.block_size;
        let start = block * blocks.block_size;
        let end = (start + blocks.block_size).min(self.size);
        let bytes = blocks
            .cache
            .try_get_with((self.id, block), || {
                blocks.runtime.block_on(blocks.store.get_range(&self.path, start as usize..end as usize))
            })
            .map_err(|e| strata_git::Error::Io(format!("reading {}: {e}", self.path)))?;
        Ok(bytes.slice((offset - start) as usize..))
    }
}

struct Objects(Cache<(u64, u64), Object>);

impl ObjectCache for Objects {
    fn get(&self, pack: u64, offset: u64) -> Option<Object> {
        self.0.get(&(pack, offset))
    }

    fn put(&self, pack: u64, offset: u64, object: &Object) {
        if object.kind != ObjectKind::Blob || object.data.len() <= MAX_CACHED_BLOB {
            self.0.insert((pack, offset), object.clone());
        }
    }
}

/// A pack being streamed to object storage from a blocking thread.
///
/// Writes are forwarded in parts as they fill, so memory use stays flat
/// however large the pack is. Wrap it in a `BufWriter` when writing many
/// small pieces.
pub struct PackUpload {
    runtime: Handle,
    inner: WriteMultipart,
}

impl Write for PackUpload {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // Holds the writer back while object storage is slower than the producer.
        self.runtime.block_on(self.inner.wait_for_capacity(UPLOAD_CONCURRENCY)).map_err(std::io::Error::other)?;
        let _in_runtime = self.runtime.enter();
        self.inner.write(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PackUpload {
    /// Completes the upload. Until this returns, the object does not exist.
    pub fn finish(self) -> anyhow::Result<()> {
        self.runtime.block_on(self.inner.finish()).context("completing pack upload")?;
        Ok(())
    }
}

/// How much of a pack is handed to an upload at a time.
const UPLOAD_PIECE: usize = 4 << 20;

/// Immutable packs in object storage.
pub struct PackStore {
    blocks: Arc<Blocks>,
    objects: Arc<Objects>,
    /// Packs whose index and sidecars are loaded. Holds no pack data.
    /// Weighed in KiB, so that a multi-gigabyte index still fits the weigher.
    opened: Cache<String, Arc<PackFile>>,
    next_id: AtomicU64,
}

const PACK: &str = "pack";
const INDEX: &str = "idx";
const GRAPH: &str = "graph";
const BITMAP: &str = "bitmap";

fn object_path(pool_id: &str, pack_id: &str, ext: &str) -> Path {
    Path::from(format!("pools/{pool_id}/packs/pack-{pack_id}.{ext}"))
}

fn cache_key(pool_id: &str, pack_id: &str) -> String {
    format!("{pool_id}/{pack_id}")
}

fn weight(len: usize) -> u32 {
    // Count some per-entry overhead so that many tiny entries cannot blow the budget.
    u32::try_from(len + 64).unwrap_or(u32::MAX)
}

impl PackStore {
    /// Must be called from within a Tokio runtime.
    pub fn new(store: Arc<dyn ObjectStore>, config: CacheConfig) -> PackStore {
        let blocks = Cache::builder()
            .max_capacity(config.block_cache_bytes)
            .weigher(|_, block: &Bytes| weight(block.len()))
            .build();
        let objects = Cache::builder()
            .max_capacity(config.object_cache_bytes)
            .weigher(|_, object: &Object| weight(object.data.len()))
            .build();
        PackStore {
            blocks: Arc::new(Blocks {
                store,
                runtime: Handle::current(),
                cache: blocks,
                block_size: config.block_size.max(1) as u64,
            }),
            objects: Arc::new(Objects(objects)),
            opened: Cache::builder()
                .max_capacity(config.index_cache_bytes >> 10)
                .weigher(|_, pack: &Arc<PackFile>| u32::try_from((pack.memory() >> 10) + 1).unwrap_or(u32::MAX))
                .build(),
            next_id: AtomicU64::new(0),
        }
    }

    /// A fresh id for a pack about to be written.
    pub fn new_pack_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    fn remote(
        &self,
        pool_id: &str,
        pack_id: &str,
        size: u64,
        index: PackIndex,
        graph: Option<CommitGraph>,
        bitmaps: Option<PackBitmaps>,
    ) -> anyhow::Result<Arc<PackFile>> {
        let data = RemotePack {
            blocks: self.blocks.clone(),
            path: object_path(pool_id, pack_id, PACK),
            size,
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
        };
        let file = PackFile::new(Arc::new(data), index, Some(self.objects.clone()))?.with_sidecars(graph, bitmaps);
        let file = Arc::new(file);
        self.opened.insert(cache_key(pool_id, pack_id), file.clone());
        Ok(file)
    }

    /// Starts streaming a pack's data to object storage.
    pub async fn upload(&self, pool_id: &str, pack_id: &str) -> anyhow::Result<PackUpload> {
        let upload =
            self.blocks.store.put_multipart(&object_path(pool_id, pack_id, PACK)).await.context("starting pack upload")?;
        Ok(PackUpload { runtime: self.blocks.runtime.clone(), inner: WriteMultipart::new(upload) })
    }

    /// Stores what describes an uploaded pack: its index and optional
    /// sidecars. The index goes last, so its presence means the pack is whole.
    ///
    /// The pack is still invisible to readers until a metadata transaction
    /// adds it to the pool's manifest.
    pub async fn finish_pack(
        &self,
        pool_id: &str,
        pack_id: &str,
        size: u64,
        index: PackIndex,
        graph: Option<CommitGraph>,
        bitmaps: Option<PackBitmaps>,
    ) -> anyhow::Result<()> {
        let store = &self.blocks.store;
        if let Some(graph) = &graph {
            store
                .put(&object_path(pool_id, pack_id, GRAPH), graph.encoded().clone().into())
                .await
                .context("uploading commit graph")?;
        }
        if let Some(bitmaps) = &bitmaps {
            store.put(&object_path(pool_id, pack_id, BITMAP), bitmaps.encode().into()).await.context("uploading bitmaps")?;
        }
        store.put(&object_path(pool_id, pack_id, INDEX), index.encoded().clone().into()).await.context("uploading pack index")?;
        self.remote(pool_id, pack_id, size, index, graph, bitmaps)?;
        Ok(())
    }

    /// Uploads a pack with its index and commit-graph, reading `data` sequentially.
    pub async fn put_pack(
        &self,
        pool_id: &str,
        pack_id: &str,
        data: Arc<dyn PackData>,
        index: PackIndex,
        graph: CommitGraph,
    ) -> anyhow::Result<()> {
        let mut upload = self.upload(pool_id, pack_id).await?;
        let size = data.size();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut offset = 0;
            while offset < size {
                let chunk = data.chunk(offset)?;
                anyhow::ensure!(!chunk.is_empty(), "pack data ended early");
                // A chunk can be all that is left of the pack. Hand it over a
                // piece at a time, so that the upload can hold the reader back.
                let piece = &chunk[..chunk.len().min(UPLOAD_PIECE)];
                upload.write_all(piece)?;
                offset += piece.len() as u64;
            }
            upload.finish()
        })
        .await??;
        self.finish_pack(pool_id, pack_id, size, index, Some(graph), None).await
    }

    /// Whether a pack's data and index are in storage right now.
    ///
    /// Asked just before a pack is added to a manifest: an upload that took
    /// longer than the orphan sweep allows may have been deleted since. A
    /// missing pack is also forgotten here, so that it can be uploaded again.
    pub async fn is_stored(&self, pool_id: &str, pack_id: &str) -> anyhow::Result<bool> {
        for ext in [INDEX, PACK] {
            match self.blocks.store.head(&object_path(pool_id, pack_id, ext)).await {
                Ok(_) => {}
                Err(object_store::Error::NotFound { .. }) => {
                    self.opened.invalidate(&cache_key(pool_id, pack_id));
                    return Ok(false);
                }
                Err(e) => return Err(e).with_context(|| format!("checking pack {pack_id}")),
            }
        }
        Ok(true)
    }

    /// Fetches a sidecar that a pack may or may not have.
    async fn optional(&self, path: &Path) -> anyhow::Result<Option<Bytes>> {
        match self.blocks.store.get(path).await {
            Ok(found) => Ok(Some(found.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn load(&self, pool_id: &str, pack_id: &str) -> anyhow::Result<Arc<PackFile>> {
        if let Some(pack) = self.opened.get(&cache_key(pool_id, pack_id)) {
            return Ok(pack);
        }
        let store = &self.blocks.store;
        let idx = store.get(&object_path(pool_id, pack_id, INDEX)).await.context("downloading pack index")?;
        let index = PackIndex::decode(idx.bytes().await?)?;
        let size = store.head(&object_path(pool_id, pack_id, PACK)).await.context("locating pack")?.size as u64;
        let graph = self.optional(&object_path(pool_id, pack_id, GRAPH)).await?.map(CommitGraph::decode).transpose()?;
        let bitmaps = self.optional(&object_path(pool_id, pack_id, BITMAP)).await?.map(PackBitmaps::decode).transpose()?;
        self.remote(pool_id, pack_id, size, index, graph, bitmaps)
    }

    /// Opens packs by id, in the order given. Only indexes and sidecars are
    /// loaded; pack data is read on demand.
    pub async fn open_each(&self, pool_id: &str, pack_ids: &[String]) -> anyhow::Result<Vec<Arc<PackFile>>> {
        let mut packs = Vec::with_capacity(pack_ids.len());
        for pack_id in pack_ids {
            packs.push(self.load(pool_id, pack_id).await?);
        }
        Ok(packs)
    }

    /// Opens the packs listed in a pool's manifest (oldest first).
    pub async fn open(&self, pool_id: &str, pack_ids: &[String]) -> anyhow::Result<PackSet> {
        Ok(PackSet::new(self.open_each(pool_id, pack_ids).await?))
    }

    /// Lists the pools that have anything in storage.
    pub async fn list_pools(&self) -> anyhow::Result<Vec<String>> {
        let listing = self.blocks.store.list_with_delimiter(Some(&Path::from("pools"))).await.context("listing pools")?;
        Ok(listing.common_prefixes.iter().filter_map(|prefix| prefix.filename().map(str::to_string)).collect())
    }

    /// Lists the packs that have any file in a pool's storage, each with the
    /// time (Unix seconds) its newest file was written. Includes packs that no
    /// manifest knows about.
    pub async fn list_stored(&self, pool_id: &str) -> anyhow::Result<Vec<(String, i64)>> {
        let prefix = Path::from(format!("pools/{pool_id}/packs"));
        let mut newest: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut listing = self.blocks.store.list(Some(&prefix));
        while let Some(entry) = listing.next().await {
            let entry = entry.context("listing packs")?;
            let Some(name) = entry.location.filename() else { continue };
            let Some(pack_id) = name.strip_prefix("pack-").and_then(|rest| rest.split('.').next()) else { continue };
            let written = entry.last_modified.timestamp();
            newest.entry(pack_id.to_string()).and_modify(|at| *at = (*at).max(written)).or_insert(written);
        }
        Ok(newest.into_iter().collect())
    }

    /// Deletes a pack and everything stored with it. Missing files are fine:
    /// a previous attempt may have got part of the way.
    pub async fn delete(&self, pool_id: &str, pack_id: &str) -> anyhow::Result<()> {
        self.opened.invalidate(&cache_key(pool_id, pack_id));
        // The index goes first: without it the pack no longer counts as present.
        for ext in [INDEX, PACK, GRAPH, BITMAP] {
            match self.blocks.store.delete(&object_path(pool_id, pack_id, ext)).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e).with_context(|| format!("deleting pack {pack_id}")),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn a_pack_is_stored_only_with_both_its_data_and_its_index() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let packs = PackStore::new(store.clone(), CacheConfig::default());
        assert!(!packs.is_stored("pool", "abc").await.unwrap());

        store.put(&object_path("pool", "abc", PACK), Bytes::from_static(b"data").into()).await.unwrap();
        assert!(!packs.is_stored("pool", "abc").await.unwrap());
        store.put(&object_path("pool", "abc", INDEX), Bytes::from_static(b"index").into()).await.unwrap();
        assert!(packs.is_stored("pool", "abc").await.unwrap());

        // What the orphan sweep does to a pack it takes for abandoned.
        packs.delete("pool", "abc").await.unwrap();
        assert!(!packs.is_stored("pool", "abc").await.unwrap());
    }
}
