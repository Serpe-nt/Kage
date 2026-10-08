//! The content-addressed artifact store (spec §18).
//!
//! Bodies, screenshots and PDFs live on disk under one directory, keyed by
//! the SHA-256 of their content:
//!
//! ```text
//! <root>/ab/cd/abcd...ef        stored as it is
//! <root>/ab/cd/abcd...ef.gz     large text, gzip-compressed
//! ```
//!
//! The same content is stored once, however many results point at it. Each
//! reference is a row in `artifacts` with its own retention deadline; the
//! retention job deletes expired rows and then every blob that no row points
//! at any more.
//!
//! Writing a blob and deleting it are serialized per content hash by a
//! Postgres advisory lock taken inside the transaction that inserts or
//! checks the rows, so a blob is never removed while a reference to it is
//! being created. Files are written to a temporary name and renamed, so a
//! reader never sees a partial blob.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use kage_core::config::ArtifactsConfig;
use kage_core::domain::{Artifact, ArtifactEncoding, ArtifactKind, ContentHash};
use kage_core::ids::{ArtifactId, RequestId};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::StoreError;
use crate::repo::ArtifactRepo;

/// Rows deleted per statement by the retention job.
const SWEEP_BATCH: i64 = 500;

/// Suffix of a blob that is being written.
const TEMP_SUFFIX: &str = ".tmp";

/// Why an artifact operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// The database part failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The file part failed.
    #[error("artifact file {}: {source}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The row exists but its blob is gone.
    #[error("artifact {id} has no blob at {}", path.display())]
    Missing {
        /// The artifact.
        id: ArtifactId,
        /// Where its blob should be.
        path: PathBuf,
    },
    /// The blob does not hash to the value it is stored under.
    #[error("artifact {id} is corrupt: its content does not match its hash")]
    Corrupt {
        /// The artifact.
        id: ArtifactId,
    },
    /// The worker thread that did the file work died.
    #[error("artifact worker failed: {0}")]
    Worker(String),
}

/// What to store.
#[derive(Debug, Clone, Copy)]
pub struct NewArtifact<'a> {
    /// What the bytes are.
    pub kind: ArtifactKind,
    /// Their MIME type, with or without parameters.
    pub content_type: &'a str,
    /// The request that produced them, if any.
    pub request_id: Option<RequestId>,
    /// The content.
    pub bytes: &'a [u8],
}

/// What one run of the retention job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Expired rows deleted.
    pub artifacts_deleted: u64,
    /// Blobs removed because no row points at them any more.
    pub blobs_deleted: u64,
    /// Bytes freed on disk.
    pub bytes_freed: u64,
}

/// The artifact store: blobs on disk, references in Postgres.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    pool: PgPool,
    retention: Duration,
    gzip_min_bytes: u64,
}

impl ArtifactStore {
    /// An artifact store rooted at the configured directory.
    pub fn new(pool: PgPool, config: &ArtifactsConfig) -> Self {
        Self {
            root: config.dir.clone(),
            pool,
            retention: config.retention(),
            gzip_min_bytes: config.gzip_min_bytes,
        }
    }

    /// The directory the blobs live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the blob for this hash and encoding lives.
    pub fn blob_path(&self, hash: &ContentHash, encoding: ArtifactEncoding) -> PathBuf {
        blob_path(&self.root, hash, encoding)
    }

    /// Stores content and returns the new artifact. If the same content is
    /// already on disk it is not written again; the artifact is a new
    /// reference to it with its own retention deadline, `now` plus the
    /// configured retention.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be written or the row cannot be inserted.
    pub async fn put(
        &self,
        new: NewArtifact<'_>,
        now: DateTime<Utc>,
    ) -> Result<Artifact, ArtifactError> {
        let encoding = self.encoding_for(new.content_type, new.bytes.len());
        let bytes = new.bytes.to_vec();
        let root = self.root.clone();

        // Hashing, compressing and writing are blocking work.
        let prepared = run_blocking(move || prepare_blob(&root, &bytes, encoding)).await?;

        let mut transaction = self.pool.begin().await.map_err(StoreError::from)?;
        ArtifactRepo::lock_blob(&mut transaction, &prepared.hash).await?;

        let final_path = self.blob_path(&prepared.hash, encoding);
        let staged = prepared.staged.clone();
        let target = final_path.clone();
        let stored_bytes = run_blocking(move || publish_blob(&staged, &target)).await?;

        let artifact = Artifact {
            id: ArtifactId::new(),
            content_hash: prepared.hash,
            kind: new.kind,
            content_type: new.content_type.to_owned(),
            size_bytes: new.bytes.len() as u64,
            stored_bytes,
            encoding,
            request_id: new.request_id,
            created_at: now,
            expires_at: now + self.retention,
        };
        ArtifactRepo::insert_on(&mut transaction, &artifact).await?;
        transaction.commit().await.map_err(StoreError::from)?;
        Ok(artifact)
    }

    /// The artifact's row, without its content.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn get(&self, id: ArtifactId) -> Result<Option<Artifact>, ArtifactError> {
        Ok(ArtifactRepo::new(&self.pool).get(id).await?)
    }

    /// The artifact and its content, decompressed and checked against its
    /// hash. `None` when there is no such artifact.
    ///
    /// # Errors
    ///
    /// Fails when the blob is missing, unreadable or does not match its
    /// hash.
    pub async fn read(&self, id: ArtifactId) -> Result<Option<(Artifact, Vec<u8>)>, ArtifactError> {
        let Some(artifact) = self.get(id).await? else {
            return Ok(None);
        };
        let path = self.blob_path(&artifact.content_hash, artifact.encoding);
        let encoding = artifact.encoding;
        let expected = artifact.content_hash.clone();

        let content = run_blocking(move || read_blob(&path, encoding)).await;
        let content = match content {
            Err(ArtifactError::Io { path, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(ArtifactError::Missing { id, path });
            }
            other => other?,
        };

        let verified = run_blocking(move || {
            let matches = hash_of(&content) == expected;
            Ok((content, matches))
        })
        .await?;
        match verified {
            (content, true) => Ok(Some((artifact, content))),
            (_, false) => Err(ArtifactError::Corrupt { id }),
        }
    }

    /// The retention job: deletes every artifact that has expired by `now`,
    /// then every blob that no artifact points at any more.
    ///
    /// # Errors
    ///
    /// Fails when a query fails or a blob cannot be removed. Rows deleted
    /// before the failure stay deleted; their blobs are picked up by
    /// [`ArtifactStore::sweep_orphans`].
    pub async fn sweep(&self, now: DateTime<Utc>) -> Result<SweepReport, ArtifactError> {
        let repo = ArtifactRepo::new(&self.pool);
        let mut report = SweepReport::default();

        loop {
            let mut blobs = repo.delete_expired(now, SWEEP_BATCH).await?;
            if blobs.is_empty() {
                break;
            }
            report.artifacts_deleted += blobs.len() as u64;
            blobs.sort();
            blobs.dedup();
            for (hash, encoding) in blobs {
                if let Some(freed) = self.remove_if_unreferenced(&hash, encoding).await? {
                    report.blobs_deleted += 1;
                    report.bytes_freed += freed;
                }
            }
        }

        if report != SweepReport::default() {
            tracing::info!(
                artifacts_deleted = report.artifacts_deleted,
                blobs_deleted = report.blobs_deleted,
                bytes_freed = report.bytes_freed,
                "artifact retention sweep"
            );
        }
        Ok(report)
    }

    /// Removes blobs that no row points at and that were last modified
    /// before `older_than`: what a crash between writing a blob and
    /// committing its row leaves behind, and temporary files of writes that
    /// never finished. The age limit keeps it away from writes in progress.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be read or a query fails.
    pub async fn sweep_orphans(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<SweepReport, ArtifactError> {
        let root = self.root.clone();
        let cutoff = std::time::SystemTime::from(older_than);
        let candidates = run_blocking(move || list_blobs(&root, cutoff)).await?;

        let mut report = SweepReport::default();
        for candidate in candidates {
            let freed = if let Some((hash, encoding)) = candidate.blob {
                self.remove_if_unreferenced(&hash, encoding).await?
            } else {
                // A leftover temporary file: nothing can reference it.
                let path = candidate.path;
                run_blocking(move || remove_file(&path)).await?
            };
            if let Some(freed) = freed {
                report.blobs_deleted += 1;
                report.bytes_freed += freed;
            }
        }
        Ok(report)
    }

    /// Removes one blob unless a row points at it. Returns the bytes freed,
    /// or `None` when the blob is still referenced or already gone.
    async fn remove_if_unreferenced(
        &self,
        hash: &ContentHash,
        encoding: ArtifactEncoding,
    ) -> Result<Option<u64>, ArtifactError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::from)?;
        ArtifactRepo::lock_blob(&mut transaction, hash).await?;
        let freed = if ArtifactRepo::blob_in_use_on(&mut transaction, hash, encoding).await? {
            None
        } else {
            let path = self.blob_path(hash, encoding);
            run_blocking(move || remove_file(&path)).await?
        };
        transaction.commit().await.map_err(StoreError::from)?;
        Ok(freed)
    }

    fn encoding_for(&self, content_type: &str, len: usize) -> ArtifactEncoding {
        if is_text(content_type) && len as u64 >= self.gzip_min_bytes {
            ArtifactEncoding::Gzip
        } else {
            ArtifactEncoding::Identity
        }
    }
}

/// Runs blocking file work off the async worker threads.
async fn run_blocking<T, F>(work: F) -> Result<T, ArtifactError>
where
    F: FnOnce() -> Result<T, ArtifactError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ArtifactError::Worker(error.to_string()))?
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> ArtifactError + '_ {
    move |source| ArtifactError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn hash_of(bytes: &[u8]) -> ContentHash {
    ContentHash::from_digest(Sha256::digest(bytes).into())
}

fn blob_path(root: &Path, hash: &ContentHash, encoding: ArtifactEncoding) -> PathBuf {
    let hex = hash.as_str();
    let mut name = hex.to_owned();
    if encoding == ArtifactEncoding::Gzip {
        name.push_str(".gz");
    }
    // A content hash is 64 ASCII hex characters, so these slices are whole.
    root.join(&hex[..2]).join(&hex[2..4]).join(name)
}

/// Whether a MIME type is text that compresses well.
fn is_text(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/x-javascript"
                | "application/x-ndjson"
                | "application/x-www-form-urlencoded"
        )
}

/// A blob hashed and written under a temporary name.
struct PreparedBlob {
    hash: ContentHash,
    staged: PathBuf,
}

/// Hashes the content and writes it, encoded, to a temporary file in the
/// directory the blob will live in.
fn prepare_blob(
    root: &Path,
    bytes: &[u8],
    encoding: ArtifactEncoding,
) -> Result<PreparedBlob, ArtifactError> {
    let hash = hash_of(bytes);
    let target = blob_path(root, &hash, encoding);
    let directory = target.parent().unwrap_or(root);
    std::fs::create_dir_all(directory).map_err(io_error(directory))?;

    let staged = directory.join(format!(
        ".{}.{}{TEMP_SUFFIX}",
        hash.as_str(),
        Uuid::now_v7().simple()
    ));
    let file = std::fs::File::create(&staged).map_err(io_error(&staged))?;
    let written = match encoding {
        ArtifactEncoding::Identity => {
            let mut file = file;
            file.write_all(bytes).and_then(|()| file.sync_all())
        }
        ArtifactEncoding::Gzip => {
            let mut encoder = GzEncoder::new(file, Compression::default());
            encoder
                .write_all(bytes)
                .and_then(|()| encoder.finish())
                .and_then(|file| file.sync_all())
        }
    };
    if let Err(source) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(io_error(&staged)(source));
    }
    Ok(PreparedBlob { hash, staged })
}

/// Moves a staged blob into place, or drops it when the blob already exists.
/// Returns the size of the blob on disk. Called with the blob's lock held.
fn publish_blob(staged: &Path, target: &Path) -> Result<u64, ArtifactError> {
    if target.exists() {
        // Same hash, same encoding: the content is already there.
        std::fs::remove_file(staged).map_err(io_error(staged))?;
    } else {
        std::fs::rename(staged, target).map_err(io_error(target))?;
    }
    let metadata = std::fs::metadata(target).map_err(io_error(target))?;
    Ok(metadata.len())
}

fn read_blob(path: &Path, encoding: ArtifactEncoding) -> Result<Vec<u8>, ArtifactError> {
    let stored = std::fs::read(path).map_err(io_error(path))?;
    match encoding {
        ArtifactEncoding::Identity => Ok(stored),
        ArtifactEncoding::Gzip => {
            let mut content = Vec::new();
            GzDecoder::new(stored.as_slice())
                .read_to_end(&mut content)
                .map_err(io_error(path))?;
            Ok(content)
        }
    }
}

/// Removes a file and returns its size, or `None` when it was already gone.
fn remove_file(path: &Path) -> Result<Option<u64>, ArtifactError> {
    let size = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path)(source)),
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(Some(size)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(path)(source)),
    }
}

/// A file found under the artifact root.
struct BlobFile {
    path: PathBuf,
    /// The blob it is, or `None` for a temporary file.
    blob: Option<(ContentHash, ArtifactEncoding)>,
}

/// Lists blobs and temporary files last modified before `cutoff`. Files
/// with any other name are left alone: they are not ours.
fn list_blobs(root: &Path, cutoff: std::time::SystemTime) -> Result<Vec<BlobFile>, ArtifactError> {
    let mut found = Vec::new();
    for first in subdirectories(root)? {
        for second in subdirectories(&first)? {
            let entries = std::fs::read_dir(&second).map_err(io_error(&second))?;
            for entry in entries {
                let entry = entry.map_err(io_error(&second))?;
                let path = entry.path();
                let metadata = entry.metadata().map_err(io_error(&path))?;
                let modified = metadata.modified().map_err(io_error(&path))?;
                if !metadata.is_file() || modified >= cutoff {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let blob = if name.ends_with(TEMP_SUFFIX) && name.starts_with('.') {
                    None
                } else if let Some(hex) = name.strip_suffix(".gz") {
                    match hex.parse() {
                        Ok(hash) => Some((hash, ArtifactEncoding::Gzip)),
                        Err(_) => continue,
                    }
                } else {
                    match name.parse() {
                        Ok(hash) => Some((hash, ArtifactEncoding::Identity)),
                        Err(_) => continue,
                    }
                };
                found.push(BlobFile { path, blob });
            }
        }
    }
    Ok(found)
}

fn subdirectories(directory: &Path) -> Result<Vec<PathBuf>, ArtifactError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // Nothing has been stored yet.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(directory)(source)),
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(directory))?;
        let file_type = entry.file_type().map_err(io_error(directory))?;
        if file_type.is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kage_core::domain::{ArtifactEncoding, ContentHash};

    use super::{blob_path, hash_of, is_text, prepare_blob, publish_blob, read_blob, remove_file};

    #[test]
    fn text_types_are_recognized_with_or_without_parameters() {
        for content_type in [
            "text/html",
            "text/html; charset=utf-8",
            "TEXT/PLAIN",
            "application/json",
            "application/json;charset=UTF-8",
            "application/ld+json",
            "application/xhtml+xml",
            "image/svg+xml",
            "application/javascript",
            "application/x-ndjson",
        ] {
            assert!(is_text(content_type), "{content_type}");
        }
        for content_type in [
            "image/png",
            "image/webp",
            "application/pdf",
            "application/octet-stream",
            "font/woff2",
            "",
        ] {
            assert!(!is_text(content_type), "{content_type}");
        }
    }

    #[test]
    fn blobs_are_fanned_out_by_hash_prefix() {
        let hash: ContentHash = format!("abcd{}", "0".repeat(60)).parse().unwrap();
        let root = Path::new("/var/lib/kage/artifacts");

        assert_eq!(
            blob_path(root, &hash, ArtifactEncoding::Identity),
            root.join("ab").join("cd").join(hash.as_str())
        );
        assert_eq!(
            blob_path(root, &hash, ArtifactEncoding::Gzip),
            root.join("ab").join("cd").join(format!("{hash}.gz"))
        );
    }

    #[test]
    fn hashing_is_sha256_of_the_content() {
        // SHA-256 of the empty input and of "abc" (FIPS 180-2 test vector).
        assert_eq!(
            hash_of(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_of(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_blob_round_trips_through_each_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let content = "<p>hello</p>\n".repeat(500).into_bytes();

        for encoding in [ArtifactEncoding::Identity, ArtifactEncoding::Gzip] {
            let prepared = prepare_blob(dir.path(), &content, encoding).unwrap();
            assert_eq!(prepared.hash, hash_of(&content));
            let target = blob_path(dir.path(), &prepared.hash, encoding);
            assert!(!target.exists());

            let stored = publish_blob(&prepared.staged, &target).unwrap();

            assert!(target.exists());
            assert!(!prepared.staged.exists());
            assert_eq!(read_blob(&target, encoding).unwrap(), content);
            match encoding {
                ArtifactEncoding::Identity => assert_eq!(stored, content.len() as u64),
                ArtifactEncoding::Gzip => assert!(stored < content.len() as u64 / 10, "{stored}"),
            }
        }
    }

    #[test]
    fn publishing_the_same_content_twice_keeps_one_blob() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"same bytes";

        let first = prepare_blob(dir.path(), content, ArtifactEncoding::Identity).unwrap();
        let target = blob_path(dir.path(), &first.hash, ArtifactEncoding::Identity);
        publish_blob(&first.staged, &target).unwrap();
        let second = prepare_blob(dir.path(), content, ArtifactEncoding::Identity).unwrap();
        assert_ne!(first.staged, second.staged);
        let size = publish_blob(&second.staged, &target).unwrap();

        assert_eq!(size, content.len() as u64);
        assert!(!second.staged.exists());
        let files: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn removing_reports_the_size_or_that_nothing_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"12345").unwrap();

        assert_eq!(remove_file(&path).unwrap(), Some(5));
        assert_eq!(remove_file(&path).unwrap(), None);
    }

    #[test]
    fn a_corrupt_gzip_blob_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.gz");
        std::fs::write(&path, b"this is not gzip").unwrap();

        assert!(read_blob(&path, ArtifactEncoding::Gzip).is_err());
        assert!(read_blob(&dir.path().join("absent"), ArtifactEncoding::Identity).is_err());
    }
}
