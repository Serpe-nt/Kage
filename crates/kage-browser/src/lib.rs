//! The browser path's side of the sidecar contract (spec §10.2): the types
//! of the contract, a client for the sidecars, and the two things Kage does
//! with what a sidecar returns before anything else looks at it.
//!
//! # Types
//!
//! [`contract`] is generated at build time from the committed OpenAPI
//! document of the sidecar (`sidecar/packages/contract/openapi.json`),
//! which is itself emitted from the sidecar's zod schemas. The two sides
//! cannot drift apart silently: the sidecar's tests fail when the document
//! is not what its schemas emit, this crate does not compile against a
//! document its code no longer fits, and the contract test reads every
//! example of the document into these types and writes it back unchanged.
//!
//! # Client
//!
//! [`SidecarClient`] calls one sidecar over the compose network with the
//! shared secret. A call is tried again only when the connection could not
//! be made at all; a request that reached the sidecar is never repeated,
//! because a navigation is not idempotent: it is a visit a site has seen.
//! A flow answers with one event per line; [`SidecarClient::flow`] returns
//! them in order and tells a flow that broke on the way
//! ([`BrowserError::FlowInterrupted`]) from one that ran to its end.
//!
//! # After a navigation
//!
//! A browser follows redirects by itself, so Kage cannot check the hops of
//! a browser navigation one by one. [`ssrf::check_chain`] checks them
//! afterwards with the same functions that guard the http path
//! ([`kage_core::ssrf`]), and [`SidecarClient::navigate_checked`] drops
//! everything a refused navigation captured. Loopback is no way around it:
//! the engines launch with a bypass list that sends loopback through the
//! proxy too.
//!
//! [`cookies`] converts between the contract's cookies and the rows of
//! `http_cookies`, so that state moves between the browser and the http
//! path without loss (spec §6.4).

#![forbid(unsafe_code)]

mod client;
pub mod cookies;
mod error;
pub mod ssrf;

/// The types of the sidecar contract, generated from its OpenAPI document.
#[allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::derive_partial_eq_without_eq,
    clippy::missing_errors_doc
)]
pub mod contract {
    include!(concat!(env!("OUT_DIR"), "/contract.rs"));
}

pub use client::{
    Closed, IMPLEMENTED_OPERATIONS, Navigation, SidecarClient, SidecarSettings, navigation_bound,
    trace_context_from,
};
pub use error::BrowserError;

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
