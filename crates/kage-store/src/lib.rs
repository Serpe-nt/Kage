//! sqlx repositories, migrations and the content-addressed artifact store.
//!
//! Scaffold only. Migrations and repositories land in milestone M1 (see
//! `docs/02-plan.md`). `sqlx` is already a dependency so that its rustls-only
//! feature set is resolved, compiled and checked by the `openssl-sys` gate from
//! the first commit.

#![forbid(unsafe_code)]

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
