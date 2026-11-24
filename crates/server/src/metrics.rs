//! What the node counts and times, for Prometheus to read at `/metrics`.
//!
//! Labels name kinds of work, never repositories: there may be millions of
//! those, and each label value is a time series someone has to store.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::StatusCode;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    UploadPart,
};
use prometheus::{Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};
use strata_store::{Event, Garbage, MetaError, MetaStore, RefScope, RepoState, Txn};
use tokio_stream::StreamExt;

/// From a ref lookup to a large clone or compaction.
const SECONDS: &[f64] = &[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 1800.0];

pub struct Metrics {
    registry: Registry,
    requests: IntCounterVec,
    request_seconds: HistogramVec,
    in_flight: IntGauge,
    pushes: IntCounterVec,
    push_bytes: IntCounter,
    push_wait_seconds: Histogram,
    fetches: IntCounterVec,
    fetch_bytes: IntCounter,
    fetch_seconds: Histogram,
    ssh: IntCounterVec,
    lfs: IntCounterVec,
    lfs_bytes: IntCounterVec,
    ssh_seconds: HistogramVec,
    compactions: IntCounterVec,
    compaction_seconds: Histogram,
    compaction_objects: IntCounter,
    pub(crate) compactions_pending: IntGauge,
    hooks: IntCounterVec,
    hook_seconds: HistogramVec,
    authz_cache: IntCounterVec,
    meta: IntCounterVec,
    meta_seconds: HistogramVec,
    store: IntCounterVec,
    store_seconds: HistogramVec,
    store_bytes: IntCounterVec,
    pub(crate) cache_bytes: IntGaugeVec,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics::new()
    }
}

impl Metrics {
    pub fn new() -> Metrics {
        let registry = Registry::new();
        let counters = |name: &str, help: &str, labels: &[&str]| {
            let metric = IntCounterVec::new(Opts::new(name, help), labels).expect("metric names are static");
            registry.register(Box::new(metric.clone())).expect("metric names are distinct");
            metric
        };
        let counter = |name: &str, help: &str| {
            let metric = IntCounter::new(name, help).expect("metric names are static");
            registry.register(Box::new(metric.clone())).expect("metric names are distinct");
            metric
        };
        let gauge = |name: &str, help: &str| {
            let metric = IntGauge::new(name, help).expect("metric names are static");
            registry.register(Box::new(metric.clone())).expect("metric names are distinct");
            metric
        };
        let histograms = |name: &str, help: &str, labels: &[&str]| {
            let metric = HistogramVec::new(HistogramOpts::new(name, help).buckets(SECONDS.to_vec()), labels).expect("metric names are static");
            registry.register(Box::new(metric.clone())).expect("metric names are distinct");
            metric
        };
        let histogram = |name: &str, help: &str| {
            let metric = Histogram::with_opts(HistogramOpts::new(name, help).buckets(SECONDS.to_vec())).expect("metric names are static");
            registry.register(Box::new(metric.clone())).expect("metric names are distinct");
            metric
        };

        let build = prometheus::IntGaugeVec::new(Opts::new("strata_build_info", "The version of this node; always 1."), &["version"]).expect("metric names are static");
        registry.register(Box::new(build.clone())).expect("metric names are distinct");
        build.with_label_values(&[env!("CARGO_PKG_VERSION")]).set(1);
        let cache_bytes = IntGaugeVec::new(Opts::new("strata_cache_bytes", "Memory held by the node's caches of pack blocks, objects and pack indexes."), &["cache"]).expect("metric names are static");
        registry.register(Box::new(cache_bytes.clone())).expect("metric names are distinct");
        #[cfg(target_os = "linux")]
        registry.register(Box::new(prometheus::process_collector::ProcessCollector::for_self())).expect("metric names are distinct");

        Metrics {
            requests: counters("strata_http_requests_total", "HTTP requests answered, by what they asked for and the status of the answer.", &["handler", "status"]),
            request_seconds: histograms(
                "strata_http_request_duration_seconds",
                "Time from a request's arrival to the start of its answer. A fetch streams its pack after that: see strata_fetch_duration_seconds.",
                &["handler"],
            ),
            in_flight: gauge("strata_http_requests_in_flight", "Requests whose answer has not started yet."),
            pushes: counters("strata_pushes_total", "Pushes, by whether their ref updates were accepted, rejected, or failed on the server.", &["result"]),
            push_bytes: counter("strata_push_bytes_total", "Bytes of pack data received in pushes."),
            push_wait_seconds: histogram("strata_push_wait_seconds", "Time pushes were held back for compaction to catch up (--wait-at)."),
            fetches: counters("strata_fetches_total", "Fetch responses, by whether they were written to the end.", &["result"]),
            fetch_bytes: counter("strata_fetch_bytes_total", "Bytes sent in fetch responses."),
            fetch_seconds: histogram("strata_fetch_duration_seconds", "Time to produce and send a fetch response, at the pace the client reads it."),
            ssh: counters("strata_ssh_commands_total", "Commands run over SSH, by service and whether they ran to the end (ok), were turned down (refused) or lost their client (aborted).", &["service", "result"]),
            ssh_seconds: histograms("strata_ssh_command_duration_seconds", "Time from a command's arrival over SSH to its end, at the pace of the client.", &["service"]),
            lfs: counters("strata_lfs_objects_total", "Large files (Git LFS) sent out (download) and taken in (upload).", &["op"]),
            lfs_bytes: counters("strata_lfs_bytes_total", "Bytes of large files sent out and taken in.", &["op"]),
            compactions: counters("strata_compactions_total", "Compaction runs, by whether they merged packs, found nothing to do, or failed.", &["result"]),
            compaction_seconds: histogram("strata_compaction_duration_seconds", "Time taken by compaction runs."),
            compaction_objects: counter("strata_compaction_objects_total", "Objects written into packs by compaction."),
            compactions_pending: gauge("strata_compactions_pending", "Repositories waiting for this node to compact them."),
            hooks: counters("strata_hook_requests_total", "Questions put to the application's hooks, by hook and whether an answer came.", &["hook", "result"]),
            hook_seconds: histograms("strata_hook_duration_seconds", "Time the application's hooks took to answer.", &["hook"]),
            authz_cache: counters("strata_authz_cache_total", "Git requests decided from a remembered answer of the authorization hook (hit) or by asking it (miss).", &["result"]),
            meta: counters("strata_meta_requests_total", "Calls to the metadata store. A conflict is a change that lost to a concurrent one.", &["op", "result"]),
            meta_seconds: histograms("strata_meta_duration_seconds", "Time taken by calls to the metadata store.", &["op"]),
            store: counters("strata_object_store_requests_total", "Calls to object storage.", &["op", "result"]),
            store_seconds: histograms("strata_object_store_duration_seconds", "Time taken by calls to object storage. For a streamed read, until the stream starts.", &["op"]),
            store_bytes: counters("strata_object_store_bytes_total", "Bytes read from and written to object storage.", &["direction"]),
            cache_bytes,
            registry,
        }
    }

    /// Everything, in the text format Prometheus reads.
    pub(crate) fn render(&self) -> String {
        TextEncoder::new().encode_to_string(&self.registry.gather()).unwrap_or_default()
    }

    /// Counts a request as in flight until what is returned is dropped.
    pub(crate) fn begin(&self) -> InFlight {
        self.in_flight.inc();
        InFlight(self.in_flight.clone())
    }

    pub(crate) fn request(&self, handler: &str, status: StatusCode, took: Duration) {
        self.requests.with_label_values(&[handler, status.as_str()]).inc();
        self.request_seconds.with_label_values(&[handler]).observe(took.as_secs_f64());
    }

    pub(crate) fn push(&self, result: &str, bytes: u64) {
        self.pushes.with_label_values(&[result]).inc();
        self.push_bytes.inc_by(bytes);
    }

    pub(crate) fn push_waited(&self, waited: Duration) {
        self.push_wait_seconds.observe(waited.as_secs_f64());
    }

    pub(crate) fn fetch(&self, result: &str, bytes: u64, took: Duration) {
        self.fetches.with_label_values(&[result]).inc();
        self.fetch_bytes.inc_by(bytes);
        self.fetch_seconds.observe(took.as_secs_f64());
    }

    pub(crate) fn ssh(&self, service: &str, result: &str, took: Duration) {
        self.ssh.with_label_values(&[service, result]).inc();
        self.ssh_seconds.with_label_values(&[service]).observe(took.as_secs_f64());
    }

    pub(crate) fn lfs(&self, op: &str, bytes: u64) {
        self.lfs.with_label_values(&[op]).inc();
        self.lfs_bytes.with_label_values(&[op]).inc_by(bytes);
    }

    pub(crate) fn compaction(&self, result: &str, objects: usize, took: Duration) {
        self.compactions.with_label_values(&[result]).inc();
        self.compaction_objects.inc_by(objects as u64);
        self.compaction_seconds.observe(took.as_secs_f64());
    }

    pub(crate) fn hook(&self, hook: &str, answered: bool, took: Duration) {
        self.hooks.with_label_values(&[hook, if answered { "ok" } else { "error" }]).inc();
        self.hook_seconds.with_label_values(&[hook]).observe(took.as_secs_f64());
    }

    pub(crate) fn authz_cache(&self, hit: bool) {
        self.authz_cache.with_label_values(&[if hit { "hit" } else { "miss" }]).inc();
    }

    fn called(&self, counts: &IntCounterVec, seconds: &HistogramVec, op: &str, result: &str, started: Instant) {
        counts.with_label_values(&[op, result]).inc();
        seconds.with_label_values(&[op]).observe(started.elapsed().as_secs_f64());
    }

    /// The same metadata store, with every call to it counted and timed.
    pub(crate) fn metered_meta(self: &Arc<Self>, inner: Arc<dyn MetaStore>) -> Arc<dyn MetaStore> {
        Arc::new(MeteredMeta { inner, metrics: self.clone() })
    }

    /// The same object storage, with every call to it counted and timed.
    pub fn metered_store(self: &Arc<Self>, inner: Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
        Arc::new(MeteredStore { inner, metrics: self.clone() })
    }
}

pub(crate) struct InFlight(IntGauge);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// What a request asks for, as the `handler` label says it. `None` for what
/// is not worth counting: the probes and the scrapes themselves.
pub(crate) fn handler(path: &str) -> Option<&'static str> {
    let path = path.trim_start_matches('/');
    if matches!(path, "healthz" | "readyz" | "metrics") {
        return None;
    }
    if path == "api/events" {
        return Some("events");
    }
    if let Some(rest) = path.strip_prefix("api/repos/") {
        return Some(match rest.rsplit_once('/').map(|(_, view)| view) {
            Some("import") => "import",
            Some("merge") => "merge",
            Some("compact") => "compact",
            _ if rest.contains("/lfs/") => "lfs",
            Some(view) if crate::content::VIEWS.contains(&view) => "content",
            _ => "repos",
        });
    }
    Some(if path.contains("/info/lfs/") {
        "lfs"
    } else if path.ends_with("/info/refs") {
        "info-refs"
    } else if path.ends_with("/git-upload-pack") {
        "upload-pack"
    } else if path.ends_with("/git-receive-pack") {
        "receive-pack"
    } else {
        "other"
    })
}

struct MeteredMeta {
    inner: Arc<dyn MetaStore>,
    metrics: Arc<Metrics>,
}

impl MeteredMeta {
    fn done<T>(&self, op: &str, started: Instant, result: Result<T, MetaError>) -> Result<T, MetaError> {
        let outcome = match &result {
            Ok(_) => "ok",
            Err(MetaError::Conflict { .. } | MetaError::PackConflict | MetaError::Stale) => "conflict",
            // The caller asked for something that is not there, or is already.
            Err(MetaError::NotFound | MetaError::AlreadyExists | MetaError::BadCursor) => "refused",
            Err(MetaError::Backend(_)) => "error",
        };
        self.metrics.called(&self.metrics.meta, &self.metrics.meta_seconds, op, outcome, started);
        result
    }
}

#[async_trait]
impl MetaStore for MeteredMeta {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError> {
        let started = Instant::now();
        self.done("create_repo", started, self.inner.create_repo(name).await)
    }
    async fn get_repo(&self, name: &str, refs: &RefScope) -> Result<Option<RepoState>, MetaError> {
        let started = Instant::now();
        self.done("get_repo", started, self.inner.get_repo(name, refs).await)
    }
    async fn list_repos(&self) -> Result<Vec<String>, MetaError> {
        let started = Instant::now();
        self.done("list_repos", started, self.inner.list_repos().await)
    }
    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError> {
        let started = Instant::now();
        self.done("commit", started, self.inner.commit(name, txn).await)
    }
    async fn delete_repo(&self, name: &str) -> Result<bool, MetaError> {
        let started = Instant::now();
        self.done("delete_repo", started, self.inner.delete_repo(name).await)
    }
    async fn list_events(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>, MetaError> {
        let started = Instant::now();
        self.done("list_events", started, self.inner.list_events(after, limit).await)
    }
    async fn purge_events(&self, created_before: i64) -> Result<(), MetaError> {
        let started = Instant::now();
        self.done("purge_events", started, self.inner.purge_events(created_before).await)
    }
    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError> {
        let started = Instant::now();
        self.done("list_garbage", started, self.inner.list_garbage(removed_before).await)
    }
    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError> {
        let started = Instant::now();
        self.done("purge_garbage", started, self.inner.purge_garbage(garbage).await)
    }
    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError> {
        let started = Instant::now();
        self.done("try_lease", started, self.inner.try_lease(key, holder, ttl_secs).await)
    }
    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError> {
        let started = Instant::now();
        self.done("release_lease", started, self.inner.release_lease(key, holder).await)
    }
}

#[derive(Debug)]
struct MeteredStore {
    inner: Arc<dyn ObjectStore>,
    metrics: Arc<Metrics>,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Metrics")
    }
}

impl fmt::Display for MeteredStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// How a call to the object store ended, as a label.
fn store_outcome<T>(result: &object_store::Result<T>) -> &'static str {
    match result {
        Ok(_) => "ok",
        // Asked for on purpose, to learn whether something is there.
        Err(object_store::Error::NotFound { .. }) => "not_found",
        Err(_) => "error",
    }
}

impl MeteredStore {
    fn done<T>(&self, op: &str, started: Instant, result: object_store::Result<T>) -> object_store::Result<T> {
        self.metrics.called(&self.metrics.store, &self.metrics.store_seconds, op, store_outcome(&result), started);
        result
    }

    fn read(&self, bytes: u64) {
        self.metrics.store_bytes.with_label_values(&["read"]).inc_by(bytes);
    }

    fn written(&self, bytes: usize) {
        self.metrics.store_bytes.with_label_values(&["written"]).inc_by(bytes as u64);
    }
}

#[async_trait]
impl ObjectStore for MeteredStore {
    async fn put_opts(&self, location: &Path, payload: PutPayload, opts: PutOptions) -> object_store::Result<PutResult> {
        let (started, bytes) = (Instant::now(), payload.content_length());
        let result = self.done("put", started, self.inner.put_opts(location, payload, opts).await);
        if result.is_ok() {
            self.written(bytes);
        }
        result
    }

    async fn put_multipart_opts(&self, location: &Path, opts: PutMultipartOptions) -> object_store::Result<Box<dyn MultipartUpload>> {
        let started = Instant::now();
        let upload = self.done("put_multipart", started, self.inner.put_multipart_opts(location, opts).await)?;
        Ok(Box::new(MeteredUpload { inner: upload, metrics: self.metrics.clone() }))
    }

    // Reading a range and asking whether something is there both arrive
    // here; they keep the names they have always been counted under.
    async fn get_opts(&self, location: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        let started = Instant::now();
        let (head, op) = match (options.head, options.range.is_some()) {
            (true, _) => (true, "head"),
            (false, true) => (false, "get_range"),
            (false, false) => (false, "get"),
        };
        let result = self.done(op, started, self.inner.get_opts(location, options).await);
        if let (false, Ok(got)) = (head, &result) {
            self.read(got.range.end.saturating_sub(got.range.start));
        }
        result
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> object_store::Result<Vec<Bytes>> {
        let started = Instant::now();
        let result = self.done("get_range", started, self.inner.get_ranges(location, ranges).await);
        if let Ok(parts) = &result {
            self.read(parts.iter().map(|part| part.len() as u64).sum());
        }
        result
    }

    fn delete_stream(&self, locations: BoxStream<'static, object_store::Result<Path>>) -> BoxStream<'static, object_store::Result<Path>> {
        let metrics = self.metrics.clone();
        // Each deletion is timed from the one before it.
        let mut started = Instant::now();
        Box::pin(self.inner.delete_stream(locations).map(move |result| {
            metrics.called(&metrics.store, &metrics.store_seconds, "delete", store_outcome(&result), started);
            started = Instant::now();
            result
        }))
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.metrics.store.with_label_values(&["list", "ok"]).inc();
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let started = Instant::now();
        self.done("list", started, self.inner.list_with_delimiter(prefix).await)
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> object_store::Result<()> {
        let started = Instant::now();
        self.done("copy", started, self.inner.copy_opts(from, to, options).await)
    }
}

/// An upload in parts, whose bytes are counted as they go.
#[derive(Debug)]
struct MeteredUpload {
    inner: Box<dyn MultipartUpload>,
    metrics: Arc<Metrics>,
}

#[async_trait]
impl MultipartUpload for MeteredUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        self.metrics.store_bytes.with_label_values(&["written"]).inc_by(data.content_length() as u64);
        self.inner.put_part(data)
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        let started = Instant::now();
        let result = self.inner.complete().await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        self.metrics.called(&self.metrics.store, &self.metrics.store_seconds, "complete_multipart", outcome, started);
        result
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.inner.abort().await
    }
}
