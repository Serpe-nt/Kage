//! Domain types, outcomes, policy model, pacing math and the block classifier.
//!
//! Scaffold only. The domain model lands in milestone M1 (see `docs/02-plan.md`).

#![forbid(unsafe_code)]

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-core");
    }
}
