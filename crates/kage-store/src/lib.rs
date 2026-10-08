//! sqlx repositories, migrations and the content-addressed artifact store
//! (spec §18).
//!
//! - [`Store`] owns the connection pool, applies the migrations in
//!   `migrations/`, and hands out one repository per group of tables.
//! - [`ArtifactStore`] keeps bodies, screenshots and PDFs on disk, keyed by
//!   content hash, with gzip for text and a retention job.
//!
//! Every query is checked at compile time against the schema. The checked
//! metadata is committed under `.sqlx/` at the workspace root, so building
//! needs no database; `just sqlx-prepare` regenerates it after a query or a
//! migration changes.

#![forbid(unsafe_code)]

mod artifact_store;
mod convert;
mod error;
pub mod repo;
mod store;

pub use artifact_store::{ArtifactError, ArtifactStore, NewArtifact, SweepReport};
pub use error::StoreError;
pub use store::{MigrationState, MigrationStatus, Store};

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-store");
    }
}
