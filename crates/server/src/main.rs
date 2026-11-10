use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use object_store::local::LocalFileSystem;
use object_store::prefix::PrefixStore;
use object_store::ObjectStore;
use strata_server::{router, spawn_background, App, Options};
use strata_store::{CacheConfig, PackStore, SqliteMetaStore};

#[derive(Parser)]
#[command(about = "Git server with separated storage and compute")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Holds the SQLite metadata store and, unless --object-store is given, the packs.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// Where packs are stored, as a URL: s3://bucket/prefix, gs://bucket/prefix,
    /// az://container/prefix or file:///path. Credentials come from the usual
    /// AWS_*, GOOGLE_* and AZURE_* environment variables.
    #[arg(long)]
    object_store: Option<String>,
    /// Create repositories on first push.
    #[arg(long)]
    auto_create: bool,
    /// The secret that may do anything, including issuing tokens. Without it,
    /// authentication is off: use that only on a trusted network.
    #[arg(long, env = "STRATA_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Let anyone fetch without credentials.
    #[arg(long)]
    anonymous_read: bool,
    /// Where incoming packs are spooled while they are validated [default: the system temp directory].
    #[arg(long)]
    scratch_dir: Option<PathBuf>,
    /// Compact a repository once a push leaves it with this many packs.
    #[arg(long, default_value_t = 8)]
    compact_at: usize,
    /// Keep objects no ref reaches when a whole repository is rewritten.
    #[arg(long)]
    no_prune: bool,
    /// Seconds to keep packs that compaction replaced, so requests reading them can finish.
    #[arg(long, default_value_t = 3600)]
    gc_grace_secs: u64,
    /// Seconds before a stored pack that was never committed is deleted. Must
    /// exceed the longest push or compaction.
    #[arg(long, default_value_t = 86400)]
    orphan_grace_secs: u64,
    /// Memory for cached pack blocks, in MiB.
    #[arg(long, default_value_t = 1024)]
    block_cache_mb: u64,
    /// Memory for cached commits, trees and small blobs, in MiB.
    #[arg(long, default_value_t = 256)]
    object_cache_mb: u64,
    /// Memory for loaded pack indexes, commit-graphs and bitmaps, in MiB.
    #[arg(long, default_value_t = 2048)]
    index_cache_mb: u64,
}

fn open_object_store(args: &Args) -> anyhow::Result<Arc<dyn ObjectStore>> {
    let Some(location) = &args.object_store else {
        let dir = args.data_dir.join("objects");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(Arc::new(LocalFileSystem::new_with_prefix(&dir)?));
    };
    let url = url::Url::parse(location).with_context(|| format!("parsing {location}"))?;
    // The cloud builders take their settings as lower-case keys.
    let settings = std::env::vars()
        .filter(|(key, _)| ["AWS_", "GOOGLE_", "AZURE_"].iter().any(|prefix| key.starts_with(prefix)))
        .map(|(key, value)| (key.to_ascii_lowercase(), value));
    let (store, prefix) = object_store::parse_url_opts(&url, settings).with_context(|| format!("opening {location}"))?;
    Ok(Arc::new(PrefixStore::new(store, prefix)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    std::fs::create_dir_all(&args.data_dir).with_context(|| format!("creating {}", args.data_dir.display()))?;
    let packs = PackStore::new(
        open_object_store(&args)?,
        CacheConfig {
            block_cache_bytes: args.block_cache_mb << 20,
            object_cache_bytes: args.object_cache_mb << 20,
            index_cache_bytes: args.index_cache_mb << 20,
            ..CacheConfig::default()
        },
    );
    if args.admin_token.is_none() {
        tracing::warn!("no admin token configured: authentication is OFF and every request is allowed");
    }
    let options = Options {
        auto_create: args.auto_create,
        scratch_dir: args.scratch_dir.unwrap_or_else(std::env::temp_dir),
        compact_at: args.compact_at,
        prune: !args.no_prune,
        gc_grace: Duration::from_secs(args.gc_grace_secs),
        orphan_grace: Duration::from_secs(args.orphan_grace_secs),
        admin_token: args.admin_token,
        anonymous_read: args.anonymous_read,
    };
    let meta = Arc::new(SqliteMetaStore::open(args.data_dir.join("meta.sqlite"))?);
    let app = Arc::new(App::new(meta, packs, options));
    spawn_background(app.clone(), Duration::from_secs(60));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(app)).await?;
    Ok(())
}
