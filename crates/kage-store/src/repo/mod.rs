//! One repository per group of tables. Each borrows the pool from
//! [`Store`](crate::Store) and maps rows to the domain types of `kage-core`.

mod access;
mod artifacts;
mod bandwidth;
mod caches;
mod cookies;
mod events;
mod fetch;
mod jobs;
mod personas;
mod proxies;
mod webhooks;

pub use access::{ApiKeyRepo, AuditRepo, SessionRepo};
pub use artifacts::ArtifactRepo;
pub use bandwidth::{BandwidthRepo, Traffic};
pub use caches::{
    ResponseCacheEntry, ResponseCacheRepo, RobotsCacheEntry, RobotsCacheRepo, SiteExitRepo,
    SiteScoutRepo,
};
pub use cookies::HttpCookieRepo;
pub use events::{BreakerRepo, HealthEventRepo, SelftestRepo};
pub use fetch::{AttemptRepo, RequestRepo, ResultRepo};
pub use jobs::{Created, Deleted, FrontierRepo, JobCount, JobItemRepo, JobRepo, Tally};
pub use personas::{BrowserUpgrade, PersonaDomainStateRepo, PersonaRepo, Upgraded};
pub use proxies::ProxyRepo;
pub use webhooks::{AttemptEnd, WebhookRepo};

/// Declares a repository: a named borrow of the pool.
macro_rules! repository {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name<'a> {
            pool: &'a ::sqlx::PgPool,
        }

        impl<'a> $name<'a> {
            pub(crate) const fn new(pool: &'a ::sqlx::PgPool) -> Self {
                Self { pool }
            }
        }
    };
}

pub(crate) use repository;

/// Largest page a listing returns, whatever the caller asks for.
pub(crate) const MAX_PAGE: u32 = 1000;

/// Clamps a caller's page size to `1..=MAX_PAGE` as the `bigint` that
/// `LIMIT` takes.
pub(crate) fn page_limit(limit: u32) -> i64 {
    i64::from(limit.clamp(1, MAX_PAGE))
}

#[cfg(test)]
mod tests {
    use super::{MAX_PAGE, page_limit};

    #[test]
    fn page_sizes_are_clamped() {
        assert_eq!(page_limit(0), 1);
        assert_eq!(page_limit(50), 50);
        assert_eq!(page_limit(u32::MAX), i64::from(MAX_PAGE));
    }
}
