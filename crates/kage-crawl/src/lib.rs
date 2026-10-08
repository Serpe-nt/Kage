//! Crawl frontier, sitemap discovery, URL normalization and dedupe.
//!
//! Scaffold only. The crawler lands in milestone M10 (see `docs/02-plan.md`).

#![forbid(unsafe_code)]

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-crawl");
    }
}
