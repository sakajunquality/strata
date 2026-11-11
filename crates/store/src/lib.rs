//! Durable state: refs and pack manifests in a transactional metadata store,
//! pack bytes in object storage.

use std::sync::Arc;

#[cfg(test)]
mod conformance;
pub mod meta;
pub mod packs;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "spanner")]
pub mod spanner;
pub mod sqlite;

pub use meta::{Event, Garbage, MemoryMetaStore, MetaError, MetaStore, RefScope, RefUpdate, RepoState, Txn};
pub use packs::{CacheConfig, PackStore, PackUpload};
#[cfg(feature = "postgres")]
pub use postgres::PostgresMetaStore;
#[cfg(feature = "spanner")]
pub use spanner::SpannerMetaStore;
pub use sqlite::SqliteMetaStore;

/// Opens the metadata store a location names:
///
/// - `sqlite:///path/to/file` (or a bare path): one file, for a single node
/// - `postgres://user:password@host/database`
/// - `spanner://projects/<p>/instances/<i>/databases/<d>`
///
/// The shared ones create their tables on first use.
pub async fn open_meta(location: &str) -> Result<Arc<dyn MetaStore>, MetaError> {
    let (scheme, rest) = location.split_once("://").unwrap_or(("sqlite", location));
    match scheme {
        "sqlite" => Ok(Arc::new(SqliteMetaStore::open(rest)?)),
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => Ok(Arc::new(PostgresMetaStore::connect(location).await?)),
        #[cfg(feature = "spanner")]
        "spanner" => Ok(Arc::new(SpannerMetaStore::connect(rest).await?)),
        other => Err(MetaError::Backend(format!(
            "unsupported metadata store: {other}://"
        ))),
    }
}
