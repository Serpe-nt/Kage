//! wreq client factory, cookie jar persistence and the HTTP fingerprint self-test.
//!
//! Scaffold only. The HTTP path lands in milestone M3 (see `docs/02-plan.md`).
//! `wreq` and `wreq-util` are already dependencies so that the BoringSSL build
//! chain runs in CI and in the image build, and the `openssl-sys` gate checks
//! the real dependency graph from the first commit.

#![forbid(unsafe_code)]

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-http");
    }
}
