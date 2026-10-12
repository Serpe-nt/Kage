//! The crawler's own logic (spec §13): what a page links to and says, which
//! links a crawl follows, and whether two pages are the same.
//!
//! Nothing here fetches or stores anything. The job worker of `kage-api`
//! drives a crawl through the scheduler like every other job; this crate is
//! what it asks.
//!
//! - [`frontier`]: every address a crawl knows, and which it reads next.
//! - [`html`]: the links and the text of a page.
//! - [`simhash`]: the content hash and the near-duplicate fingerprint.
//! - [`scope`]: which addresses a crawl reads.
//! - [`site`]: what a site is.
//! - [`sitemap`]: the list of its pages a site publishes.

#![forbid(unsafe_code)]

pub mod frontier;
pub mod html;
pub mod scope;
pub mod simhash;
pub mod site;
pub mod sitemap;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
