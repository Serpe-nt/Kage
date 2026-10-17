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
//!
//! A file the sidecar wrote is taken only as the sidecar describes it: in
//! the sidecars' part of the artifacts directory, a plain file and no link,
//! pipe or device, of the size and the SHA-256 it names (security review
//! S1). A sidecar that was taken over cannot make Kage read anything else
//! into a result: not Kage's environment or secrets, whose content it does
//! not know, nor a file that never ends.

use std::ffi::OsStr;
use std::fmt;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use kage_browser::contract::{ArtifactRef as SidecarFile, BodyEncoding, CapturedBody, PageResult};
use kage_core::domain::{ArtifactKind, ArtifactRef, ContentHash, Header};
use kage_core::ids::RequestId;
use kage_core::outcome::Outcome;
use kage_http::Fetched;
use kage_store::{ArtifactStore, NewArtifact};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::executor::AttemptOutput;

/// The largest body of a captured XHR or `fetch()` response that is kept
/// inside the result itself. A larger one, and one that is not text, is an
/// artifact of its own.
pub const INLINE_XHR_BYTES: usize = 256 * 1024;

/// The largest file a sidecar hands over: a full-page screenshot or the PDF
/// of a long page is a few tens of MiB.
pub const MAX_SIDECAR_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Where below the artifacts directory the sidecars write their files
/// (`sidecar/<id>/...`). The store keeps its own blobs below the same
/// directory, under two hexadecimal digits.
const SIDECAR_AREA: &str = "sidecar";

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

    /// A reference of `request` to a body that is already stored: what a
    /// result served from the cache points at (spec §15). Nothing is
    /// written again. `None` when the body is gone.
    async fn link(
        &self,
        request: RequestId,
        artifact: ArtifactRef,
        now: DateTime<Utc>,
    ) -> Option<ArtifactRef>;
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
pub(crate) fn bytes_of(body: &CapturedBody) -> Option<Vec<u8>> {
    match body.encoding {
        BodyEncoding::Utf8 => Some(body.data.clone().into_bytes()),
        BodyEncoding::Base64 => STANDARD.decode(body.data.as_bytes()).ok(),
    }
}

/// Where a file the sidecar named lies below the artifacts directory, or
/// `None` for a path that would leave the sidecars' part of it.
fn below(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let mut components = relative.components();
    let inside = components.next() == Some(Component::Normal(OsStr::new(SIDECAR_AREA)))
        && components.next().is_some()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    inside.then(|| root.join(relative))
}

/// Why a file a sidecar named was not taken.
#[derive(Debug, thiserror::Error)]
enum NotTaken {
    /// It says it is larger than [`MAX_SIDECAR_FILE_BYTES`].
    #[error("the sidecar says it is larger than a sidecar's file may be")]
    TooLarge,
    /// A link leads out of the sidecars' part of the artifacts directory.
    #[error("it lies outside the sidecars' part of the artifacts directory")]
    Outside,
    /// A link, a pipe, a device, a directory.
    #[error("it is not a plain file")]
    NotAFile,
    /// Not the size the sidecar named.
    #[error("it is not as long as the sidecar says")]
    Length,
    /// Not the content the sidecar named.
    #[error("its content is not what the sidecar says it is")]
    Content,
    /// It could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Opens a file to read without following a link at its last step, and
/// without waiting for a writer when it is a pipe.
#[cfg(unix)]
fn open_plain(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Opens a file to read; a link at its last step is refused.
#[cfg(not(unix))]
fn open_plain(path: &Path) -> std::io::Result<std::fs::File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("a link"));
    }
    std::fs::File::open(path)
}

/// Reads the file at `path` as `file` describes it, and says where it
/// really is, for removing it once it is stored.
fn read_described(
    root: &Path,
    path: &Path,
    file: &SidecarFile,
) -> Result<(PathBuf, Vec<u8>), NotTaken> {
    if file.bytes > MAX_SIDECAR_FILE_BYTES {
        return Err(NotTaken::TooLarge);
    }
    let area = root.join(SIDECAR_AREA).canonicalize()?;
    let real = path.canonicalize()?;
    if !real.starts_with(&area) {
        return Err(NotTaken::Outside);
    }
    let opened = open_plain(&real)?;
    let metadata = opened.metadata()?;
    if !metadata.is_file() {
        return Err(NotTaken::NotAFile);
    }
    if metadata.len() != file.bytes {
        return Err(NotTaken::Length);
    }
    let mut bytes = Vec::new();
    opened
        .take(file.bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).ok() != Some(file.bytes) {
        return Err(NotTaken::Length);
    }
    let digest = ContentHash::from_digest(Sha256::digest(&bytes).into());
    if !digest.as_str().eq_ignore_ascii_case(&file.sha256) {
        return Err(NotTaken::Content);
    }
    Ok((real, bytes))
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
            tracing::warn!(%request, kind = kind.as_str(), "a sidecar named a file outside its part of the artifacts directory");
            return None;
        };
        let root = self.store.root().to_path_buf();
        let described = file.clone();
        let read = tokio::task::spawn_blocking(move || read_described(&root, &path, &described))
            .await
            .map_err(|error| NotTaken::Io(std::io::Error::other(error)));
        let (path, bytes) = match read.and_then(|read| read) {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%request, kind = kind.as_str(), %error, "a file the sidecar wrote is not taken");
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

    async fn link(
        &self,
        request: RequestId,
        artifact: ArtifactRef,
        now: DateTime<Utc>,
    ) -> Option<ArtifactRef> {
        match self.store.link(artifact.id, request, now).await {
            Ok(linked) => linked.map(|linked| ArtifactRef {
                kind: artifact.kind,
                id: linked.id,
            }),
            Err(error) => {
                tracing::warn!(%request, artifact = %artifact.id, %error, "a stored body could not be pointed at");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kage_browser::contract::{BodyEncoding, CapturedBody};
    use kage_core::domain::Header;

    use super::{
        Keep, MAX_SIDECAR_FILE_BYTES, NotTaken, SidecarFile, below, bytes_of, content_type_of,
        read_described,
    };

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
            // The store's own blobs, and the area itself.
            "ab/cd/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "sidecar",
            "sidecars/shot.webp",
        ] {
            assert_eq!(below(root, outside), None, "{outside:?}");
        }
    }

    /// Security review S1.
    #[cfg(unix)]
    #[test]
    fn a_sidecars_file_is_taken_only_as_the_sidecar_describes_it() {
        use std::os::unix::fs::symlink;

        use kage_core::domain::ContentHash;
        use sha2::{Digest as _, Sha256};

        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let area = root.path().join("sidecar/ctx");
        std::fs::create_dir_all(&area).unwrap();
        let describe = |name: &str, content: &[u8]| SidecarFile {
            path: format!("sidecar/ctx/{name}"),
            bytes: u64::try_from(content.len()).unwrap(),
            sha256: ContentHash::from_digest(Sha256::digest(content).into())
                .as_str()
                .to_owned(),
            content_type: "image/webp".to_owned(),
        };
        let read = |file: &SidecarFile| {
            let path = below(root.path(), &file.path).unwrap();
            read_described(root.path(), &path, file)
        };

        let shot = b"RIFF\x10\x00\x00\x00WEBPVP8 a picture";
        std::fs::write(area.join("shot.webp"), shot).unwrap();
        let (found, bytes) = read(&describe("shot.webp", shot)).unwrap();
        assert_eq!(bytes, shot);
        assert_eq!(found, area.join("shot.webp").canonicalize().unwrap());

        // Another content, another length.
        let mut wrong = describe("shot.webp", b"RIFF\x10\x00\x00\x00WEBPVP8 a pictur!");
        assert!(
            matches!(read(&wrong), Err(NotTaken::Content)),
            "{:?}",
            read(&wrong)
        );
        wrong = describe("shot.webp", b"a picture");
        assert!(
            matches!(read(&wrong), Err(NotTaken::Length)),
            "{:?}",
            read(&wrong)
        );
        wrong = SidecarFile {
            bytes: MAX_SIDECAR_FILE_BYTES + 1,
            ..describe("shot.webp", shot)
        };
        assert!(
            matches!(read(&wrong), Err(NotTaken::TooLarge)),
            "{:?}",
            read(&wrong)
        );

        // A secret of the host, described as it is: a link to it is no
        // file of the sidecar's, at its last step or before it.
        let secret = b"KAGE_MASTER_KEY=0123";
        std::fs::write(elsewhere.path().join("environ"), secret).unwrap();
        symlink(elsewhere.path().join("environ"), area.join("environ")).unwrap();
        let file = describe("environ", secret);
        assert!(
            matches!(read(&file), Err(NotTaken::Outside)),
            "{:?}",
            read(&file)
        );
        symlink(elsewhere.path(), area.join("out")).unwrap();
        let file = describe("out/environ", secret);
        assert!(
            matches!(read(&file), Err(NotTaken::Outside)),
            "{:?}",
            read(&file)
        );
        // A link that stays inside is read for what it leads to; a link
        // put in place of a file after that check is not opened.
        symlink(area.join("shot.webp"), area.join("again.webp")).unwrap();
        let (found, _) = read(&describe("again.webp", shot)).unwrap();
        assert_eq!(found, area.join("shot.webp").canonicalize().unwrap());
        assert!(super::open_plain(&area.join("again.webp")).is_err());

        // A directory, and a pipe nobody writes to: refused at once.
        std::fs::create_dir(area.join("dir")).unwrap();
        let file = describe("dir", b"");
        assert!(
            matches!(read(&file), Err(NotTaken::NotAFile)),
            "{:?}",
            read(&file)
        );
        let made = std::process::Command::new("mkfifo")
            .arg(area.join("pipe"))
            .status()
            .is_ok_and(|status| status.success());
        if made {
            let file = describe("pipe", b"");
            assert!(
                matches!(read(&file), Err(NotTaken::NotAFile)),
                "{:?}",
                read(&file)
            );
        }
        // Nothing of the host was touched.
        assert_eq!(
            std::fs::read(elsewhere.path().join("environ")).unwrap(),
            secret
        );
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
