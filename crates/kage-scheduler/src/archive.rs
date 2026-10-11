//! What a request keeps of its page (spec §16.4 `include`, §18).
//!
//! An attempt returns its page in memory: the document, the raw body, the
//! responses its scripts fetched, and the files the sidecar wrote for a
//! screenshot or a PDF. A result that is read again later (`GET
//! /v1/results/{id}`, a job's item, a fetch that outlasted its wall time)
//! has none of that unless it was stored. The pipeline therefore hands the
//! last attempt of a request to an [`Archive`] before the result is
//! written, and the result lists what was kept.
//!
//! [`StoreArchive`] keeps bodies in the content-addressed artifact store.
//! Only what the request asked for is kept ([`Keep`]), nothing of an
//! attempt the SSRF guard refused, and a body that cannot be stored is a
//! warning in the log, never a failed fetch.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use kage_browser::contract::{ArtifactRef as SidecarFile, BodyEncoding, CapturedBody, PageResult};
use kage_core::domain::{ArtifactKind, ArtifactRef, Header};
use kage_core::ids::RequestId;
use kage_core::outcome::Outcome;
use kage_http::Fetched;
use kage_store::{ArtifactStore, NewArtifact};
use serde_json::{Value, json};

use crate::executor::AttemptOutput;

/// The largest body of a captured XHR or `fetch()` response that is kept
/// inside the result itself. A larger one, and one that is not text, is an
/// artifact of its own.
pub const INLINE_XHR_BYTES: usize = 256 * 1024;

/// Which parts of a page a request asked to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // Five independent parts, each kept or not.
pub struct Keep {
    /// The document as the browser had it after the page settled.
    pub dom: bool,
    /// The body of the main response as the server sent it, decoded.
    pub raw: bool,
    /// The responses the page's scripts fetched.
    pub xhr: bool,
    /// The screenshot.
    pub screenshot: bool,
    /// The PDF.
    pub pdf: bool,
}

impl Keep {
    /// Whether nothing is to be kept.
    pub const fn is_nothing(&self) -> bool {
        !(self.dom || self.raw || self.xhr || self.screenshot || self.pdf)
    }
}

/// What was kept of a page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Archived {
    /// The stored bodies and files.
    pub artifacts: Vec<ArtifactRef>,
    /// The captured XHR and `fetch()` responses: one object per response
    /// with `url`, `method`, `kind`, `status`, `content_type`, `bytes`,
    /// `truncated`, and its body either as `body` (text, when it is small)
    /// or as `body_artifact` (the id of an artifact).
    pub xhr: Option<Value>,
}

/// Keeps what a request asked to keep of its last attempt.
#[async_trait]
pub trait Archive: Send + Sync + fmt::Debug {
    /// Stores the parts of `output` that `keep` names. Never fails: what
    /// could not be stored is missing from what is returned.
    async fn keep(
        &self,
        request: RequestId,
        keep: Keep,
        output: &AttemptOutput,
        now: DateTime<Utc>,
    ) -> Archived;
}

/// The archive over the artifact store. The sidecars write their files
/// below the same directory the store keeps its blobs in (the shared
/// artifacts volume); a file is taken into the store and then removed.
#[derive(Debug, Clone)]
pub struct StoreArchive {
    store: ArtifactStore,
}

/// The media type of a response, from its headers.
fn content_type_of(headers: &[Header]) -> Option<&str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.as_str())
        .filter(|value| !value.trim().is_empty())
}

/// The bytes of a body the sidecar captured.
fn bytes_of(body: &CapturedBody) -> Option<Vec<u8>> {
    match body.encoding {
        BodyEncoding::Utf8 => Some(body.data.clone().into_bytes()),
        BodyEncoding::Base64 => STANDARD.decode(body.data.as_bytes()).ok(),
    }
}

/// Where a file the sidecar named lies below the artifacts directory, or
/// `None` for a path that would leave it.
fn below(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let inside = !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    inside.then(|| root.join(relative))
}

impl StoreArchive {
    /// An archive that keeps bodies in `store`.
    pub const fn new(store: ArtifactStore) -> Self {
        Self { store }
    }

    async fn put(
        &self,
        request: RequestId,
        kind: ArtifactKind,
        content_type: &str,
        bytes: &[u8],
        now: DateTime<Utc>,
    ) -> Option<ArtifactRef> {
        let new = NewArtifact {
            kind,
            content_type,
            request_id: Some(request),
            bytes,
        };
        match self.store.put(new, now).await {
            Ok(artifact) => Some(ArtifactRef {
                kind,
                id: artifact.id,
            }),
            Err(error) => {
                tracing::warn!(%request, kind = kind.as_str(), %error, "a body could not be stored");
                None
            }
        }
    }

    /// Takes a file the sidecar wrote into the store and removes it.
    async fn take(
        &self,
        request: RequestId,
        kind: ArtifactKind,
        file: &SidecarFile,
        now: DateTime<Utc>,
    ) -> Option<ArtifactRef> {
        let Some(path) = below(self.store.root(), &file.path) else {
            tracing::warn!(%request, kind = kind.as_str(), "a sidecar named a file outside the artifacts directory");
            return None;
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%request, kind = kind.as_str(), %error, "a file the sidecar wrote cannot be read");
                return None;
            }
        };
        let stored = self
            .put(request, kind, &file.content_type, &bytes, now)
            .await;
        if stored.is_some()
            && let Err(error) = tokio::fs::remove_file(&path).await
        {
            tracing::debug!(%request, %error, "a sidecar's file could not be removed after it was stored");
        }
        stored
    }

    async fn keep_page(
        &self,
        request: RequestId,
        keep: Keep,
        page: &PageResult,
        now: DateTime<Utc>,
    ) -> Archived {
        let mut archived = Archived::default();
        if keep.dom
            && let Some(dom) = &page.dom_html
        {
            archived.artifacts.extend(
                self.put(
                    request,
                    ArtifactKind::DomHtml,
                    "text/html; charset=utf-8",
                    dom.as_bytes(),
                    now,
                )
                .await,
            );
        }
        if keep.raw
            && let Some(body) = &page.raw_body
            && let Some(bytes) = bytes_of(body)
        {
            let headers: Vec<Header> = page
                .headers
                .iter()
                .map(|header| Header {
                    name: header.name.clone(),
                    value: header.value.clone(),
                })
                .collect();
            let content_type = content_type_of(&headers).unwrap_or("application/octet-stream");
            archived.artifacts.extend(
                self.put(request, ArtifactKind::RawBody, content_type, &bytes, now)
                    .await,
            );
        }
        if keep.screenshot
            && let Some(file) = &page.screenshot
        {
            archived.artifacts.extend(
                self.take(request, ArtifactKind::Screenshot, file, now)
                    .await,
            );
        }
        if keep.pdf
            && let Some(file) = &page.pdf
        {
            archived
                .artifacts
                .extend(self.take(request, ArtifactKind::Pdf, file, now).await);
        }
        if keep.xhr {
            let mut responses = Vec::with_capacity(page.xhr.len());
            for response in &page.xhr {
                let mut entry = json!({
                    "url": response.url,
                    "method": response.method,
                    "kind": response.kind,
                    "status": response.status,
                    "content_type": response.content_type,
                    "bytes": response.body.as_ref().map(|body| body.bytes),
                    "truncated": response.body.as_ref().is_some_and(|body| body.truncated),
                });
                if let Some(body) = &response.body {
                    let inline =
                        body.encoding == BodyEncoding::Utf8 && body.data.len() <= INLINE_XHR_BYTES;
                    if inline {
                        entry["body"] = json!(body.data);
                    } else if let Some(bytes) = bytes_of(body) {
                        let content_type = response
                            .content_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        if let Some(stored) = self
                            .put(request, ArtifactKind::XhrBody, content_type, &bytes, now)
                            .await
                        {
                            entry["body_artifact"] = json!(stored.id);
                            archived.artifacts.push(stored);
                        }
                    }
                }
                responses.push(entry);
            }
            archived.xhr = Some(json!({
                "responses": responses,
                "dropped": page.xhr_dropped,
            }));
        }
        archived
    }

    async fn keep_fetched(
        &self,
        request: RequestId,
        keep: Keep,
        fetched: &Fetched,
        now: DateTime<Utc>,
    ) -> Archived {
        let mut archived = Archived::default();
        // The http path has no document but the body, and runs no script.
        if keep.raw {
            let content_type =
                content_type_of(&fetched.headers).unwrap_or("application/octet-stream");
            archived.artifacts.extend(
                self.put(
                    request,
                    ArtifactKind::RawBody,
                    content_type,
                    &fetched.body,
                    now,
                )
                .await,
            );
        }
        archived
    }
}

#[async_trait]
impl Archive for StoreArchive {
    async fn keep(
        &self,
        request: RequestId,
        keep: Keep,
        output: &AttemptOutput,
        now: DateTime<Utc>,
    ) -> Archived {
        // Nothing read from a hop the guard refused is kept (spec §19).
        if keep.is_nothing() || output.outcome() == Outcome::SsrfDenied {
            return Archived::default();
        }
        if let Some(page) = &output.page {
            return self.keep_page(request, keep, page, now).await;
        }
        if let Some(fetched) = &output.fetched {
            return self.keep_fetched(request, keep, fetched, now).await;
        }
        Archived::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kage_browser::contract::{BodyEncoding, CapturedBody};
    use kage_core::domain::Header;

    use super::{Keep, below, bytes_of, content_type_of};

    #[test]
    fn nothing_is_kept_unless_a_part_is_named() {
        assert!(Keep::default().is_nothing());
        for keep in [
            Keep {
                dom: true,
                ..Keep::default()
            },
            Keep {
                raw: true,
                ..Keep::default()
            },
            Keep {
                xhr: true,
                ..Keep::default()
            },
            Keep {
                screenshot: true,
                ..Keep::default()
            },
            Keep {
                pdf: true,
                ..Keep::default()
            },
        ] {
            assert!(!keep.is_nothing(), "{keep:?}");
        }
    }

    #[test]
    fn a_sidecars_file_is_only_read_below_the_artifacts_directory() {
        let root = Path::new("/var/lib/kage/artifacts");

        assert_eq!(
            below(root, "sidecar/0199-ctx/shot.webp"),
            Some(root.join("sidecar/0199-ctx/shot.webp"))
        );
        for outside in [
            "",
            "../secrets",
            "sidecar/../../etc/passwd",
            "/etc/passwd",
            "./sidecar/shot.webp",
        ] {
            assert_eq!(below(root, outside), None, "{outside:?}");
        }
    }

    #[test]
    fn a_captured_body_is_text_or_base64() {
        let body = |encoding, data: &str| CapturedBody {
            bytes: 5,
            data: data.to_owned(),
            encoding,
            truncated: false,
        };

        assert_eq!(
            bytes_of(&body(BodyEncoding::Utf8, "héllo")).unwrap(),
            "héllo".as_bytes()
        );
        assert_eq!(
            bytes_of(&body(BodyEncoding::Base64, "AAEC/w==")).unwrap(),
            [0, 1, 2, 255]
        );
        assert_eq!(bytes_of(&body(BodyEncoding::Base64, "not base64!")), None);
    }

    #[test]
    fn the_media_type_is_the_responses_own() {
        let header = |name: &str, value: &str| Header {
            name: name.to_owned(),
            value: value.to_owned(),
        };

        assert_eq!(
            content_type_of(&[
                header("server", "x"),
                header("Content-Type", "application/json; charset=utf-8"),
            ]),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(content_type_of(&[header("content-type", "  ")]), None);
        assert_eq!(content_type_of(&[]), None);
    }
}
