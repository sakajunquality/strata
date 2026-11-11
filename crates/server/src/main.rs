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
use strata_store::{open_meta, CacheConfig, PackStore};

#[derive(Parser)]
#[command(about = "Git server with separated storage and compute")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Holds the metadata (unless --meta is given) and the packs (unless --object-store is given).
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// Where refs and pack lists are stored, as a URL. Several nodes need a
    /// shared one: postgres://user:password@host/database or
    /// spanner://projects/<p>/instances/<i>/databases/<d>. A single node can
    /// use sqlite:///path/to/file [default: a SQLite file in --data-dir].
    #[arg(long, env = "STRATA_META", hide_env_values = true)]
    meta: Option<String>,
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
    /// Let anyone fetch without credentials. Ignored with --authz-url.
    #[arg(long)]
    anonymous_read: bool,
    /// The application's authorization hook. Each git request is POSTed to it
    /// as {"path", "access", "authorization"}; it answers {"allow": true,
    /// "repo": "<id to serve>", "actor": "<who>"} or {"allow": false}.
    #[arg(long, env = "STRATA_AUTHZ_URL")]
    authz_url: Option<String>,
    /// Seconds to reuse an answer from the authorization hook: a revoked
    /// permission keeps working that long. 0 asks every time.
    #[arg(long, default_value_t = 10)]
    authz_cache_secs: u64,
    /// The application's push rules. Each push is POSTed to it before it is
    /// committed, as {"repo", "actor", "updates"}; it answers {"allow": true}
    /// or {"allow": false, "message": "..."}.
    #[arg(long, env = "STRATA_PRE_RECEIVE_URL")]
    pre_receive_url: Option<String>,
    /// Sent to both hooks as a Bearer token, so that they know who is asking.
    #[arg(long, env = "STRATA_HOOK_SECRET", hide_env_values = true)]
    hook_secret: Option<String>,
    /// Seconds to keep events in the feed at /api/events.
    #[arg(long, default_value_t = 604800)]
    event_retention_secs: u64,
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
    // Colours only where someone is looking: not in a file or a container's log.
    tracing_subscriber::fmt().with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())).init();
    // Two TLS backends are linked in (by different dependencies); the HTTP
    // and gRPC clients need to be told which one to use.
    let _ = rustls::crypto::ring::default_provider().install_default();
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
        // With a hook deciding git access, an open management API would be a way around it.
        anyhow::ensure!(args.authz_url.is_none(), "--authz-url needs --admin-token: the management API would be open to anyone");
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
        authz_url: args.authz_url,
        authz_cache: Duration::from_secs(args.authz_cache_secs),
        pre_receive_url: args.pre_receive_url,
        hook_secret: args.hook_secret,
        event_retention: Duration::from_secs(args.event_retention_secs),
    };
    let meta = match &args.meta {
        Some(location) => open_meta(location).await,
        None => open_meta(&args.data_dir.join("meta.sqlite").to_string_lossy()).await,
    }
    .context("opening the metadata store")?;
    let app = Arc::new(App::new(meta, packs, options));
    spawn_background(app.clone(), Duration::from_secs(60));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    // On a signal to stop, no new connections are taken and requests in
    // flight are let finish: what a rolling update needs of a node.
    axum::serve(listener, router(app)).with_graceful_shutdown(stop_signal()).await?;
    tracing::info!("stopped");
    Ok(())
}

async fn stop_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("a signal handler can be installed");
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
    tracing::info!("stopping: finishing the requests in flight");
}
