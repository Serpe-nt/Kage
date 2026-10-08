//! Sidecar contract types (generated from the sidecar OpenAPI document) and client.
//!
//! Scaffold only. The contract client lands in milestone M4 (see `docs/02-plan.md`).

#![forbid(unsafe_code)]

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-browser");
    }
}
