//! The artifact store against a real Postgres and a real directory:
//! content addressing, gzip for text, integrity checks and retention.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::Duration;
use common::{TestDb, now};
use kage_core::config::ArtifactsConfig;
use kage_core::domain::{ArtifactEncoding, ArtifactKind};
use kage_core::duration::HumanDuration;
use kage_core::ids::ArtifactId;
use kage_store::{ArtifactError, ArtifactStore, NewArtifact, SweepReport};
use tempfile::TempDir;

const RETENTION_DAYS: u32 = 14;

fn artifact_store(db: &TestDb, dir: &TempDir) -> ArtifactStore {
    let config = ArtifactsConfig {
        dir: dir.path().join("artifacts"),
        retention_days: RETENTION_DAYS,
        gzip_min_bytes: 1024,
        sweep_interval: HumanDuration::from_hours(1),
    };
    ArtifactStore::new(db.store.pool().clone(), &config)
}

fn html(paragraphs: usize) -> Vec<u8> {
    let mut page = String::from("<!doctype html><html><body>");
    for n in 0..paragraphs {
        page.push_str("<p>Paragraph number ");
        page.push_str(&n.to_string());
        page.push_str(" of an ordinary page.</p>\n");
    }
    page.push_str("</body></html>");
    page.into_bytes()
}

fn text(bytes: &[u8]) -> NewArtifact<'_> {
    NewArtifact {
        kind: ArtifactKind::DomHtml,
        content_type: "text/html; charset=utf-8",
        request_id: None,
        bytes,
    }
}

fn binary(bytes: &[u8]) -> NewArtifact<'_> {
    NewArtifact {
        kind: ArtifactKind::Screenshot,
        content_type: "image/webp",
        request_id: None,
        bytes,
    }
}

/// Every regular file under the artifact directory.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn large_text_is_stored_gzipped_under_its_content_hash() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let page = html(400);
    let stored_at = now();
    let request = common::request(None);
    db.store.requests().insert(&request).await.unwrap();

    let artifact = store
        .put(
            NewArtifact {
                request_id: Some(request.id),
                ..text(&page)
            },
            stored_at,
        )
        .await
        .unwrap();

    assert_eq!(artifact.kind, ArtifactKind::DomHtml);
    assert_eq!(artifact.content_type, "text/html; charset=utf-8");
    assert_eq!(artifact.encoding, ArtifactEncoding::Gzip);
    assert_eq!(artifact.size_bytes, page.len() as u64);
    assert!(
        artifact.stored_bytes < artifact.size_bytes / 5,
        "{artifact:?}"
    );
    assert_eq!(artifact.request_id, Some(request.id));
    assert_eq!(artifact.created_at, stored_at);
    assert_eq!(
        artifact.expires_at,
        stored_at + Duration::days(i64::from(RETENTION_DAYS))
    );

    // On disk: <root>/<2 hex>/<2 hex>/<hash>.gz, and nothing else.
    let hash = artifact.content_hash.as_str();
    let expected = store
        .root()
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(format!("{hash}.gz"));
    assert_eq!(
        store.blob_path(&artifact.content_hash, artifact.encoding),
        expected
    );
    assert_eq!(files_under(store.root()), std::slice::from_ref(&expected));
    assert_eq!(
        std::fs::metadata(&expected).unwrap().len(),
        artifact.stored_bytes
    );
    // The file is a standard gzip stream that decodes to the page.
    let on_disk = std::fs::read(&expected).unwrap();
    assert_eq!(on_disk[..2], [0x1f, 0x8b]);
    let mut unpacked = Vec::new();
    flate2::read::GzDecoder::new(on_disk.as_slice())
        .read_to_end(&mut unpacked)
        .unwrap();
    assert_eq!(unpacked, page);

    // Through the store: the row and the original bytes.
    assert_eq!(
        store.get(artifact.id).await.unwrap(),
        Some(artifact.clone())
    );
    assert_eq!(
        db.store.artifacts().get(artifact.id).await.unwrap(),
        Some(artifact.clone())
    );
    assert_eq!(
        db.store
            .artifacts()
            .list_for_request(request.id)
            .await
            .unwrap(),
        std::slice::from_ref(&artifact)
    );
    let (read_back, content) = store.read(artifact.id).await.unwrap().unwrap();
    assert_eq!(read_back, artifact);
    assert_eq!(content, page);
}

#[tokio::test]
async fn small_text_and_binary_content_are_stored_as_they_are() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let small_page = html(2);
    assert!(small_page.len() < 1024);
    // Bytes that are not valid UTF-8, well over the gzip threshold.
    let image: Vec<u8> = (0..20_000_u32).flat_map(u32::to_le_bytes).collect();

    let page = store.put(text(&small_page), now()).await.unwrap();
    let shot = store.put(binary(&image), now()).await.unwrap();

    for (artifact, original) in [(&page, &small_page), (&shot, &image)] {
        assert_eq!(artifact.encoding, ArtifactEncoding::Identity);
        assert_eq!(artifact.stored_bytes, original.len() as u64);
        let path = store.blob_path(&artifact.content_hash, artifact.encoding);
        assert_eq!(&std::fs::read(path).unwrap(), original);
        let (_, content) = store.read(artifact.id).await.unwrap().unwrap();
        assert_eq!(&content, original);
    }
    assert_eq!(shot.kind, ArtifactKind::Screenshot);
    assert_ne!(page.content_hash, shot.content_hash);
}

#[tokio::test]
async fn the_same_content_is_stored_once_and_referenced_many_times() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let page = html(300);

    let first = store.put(text(&page), now()).await.unwrap();
    let second = store.put(text(&page), now()).await.unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(first.content_hash, second.content_hash);
    assert_eq!(first.stored_bytes, second.stored_bytes);
    assert_eq!(files_under(store.root()).len(), 1);

    // Concurrent writers of one blob neither corrupt it nor leave temp files.
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let store = store.clone();
        let page = page.clone();
        tasks.push(tokio::spawn(async move {
            store.put(text(&page), now()).await.unwrap()
        }));
    }
    for task in tasks {
        let artifact = task.await.unwrap();
        assert_eq!(artifact.content_hash, first.content_hash);
        let (_, content) = store.read(artifact.id).await.unwrap().unwrap();
        assert_eq!(content, page);
    }
    assert_eq!(files_under(store.root()).len(), 1);

    // The same bytes stored as binary are a different blob: the encoding is
    // part of the address.
    let as_binary = store.put(binary(&page), now()).await.unwrap();
    assert_eq!(as_binary.content_hash, first.content_hash);
    assert_eq!(as_binary.encoding, ArtifactEncoding::Identity);
    assert_eq!(files_under(store.root()).len(), 2);
}

#[tokio::test]
async fn an_unknown_artifact_reads_as_none() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);

    assert_eq!(store.get(ArtifactId::new()).await.unwrap(), None);
    assert!(store.read(ArtifactId::new()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_missing_or_altered_blob_is_reported_not_returned() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let page = html(300);
    let image = vec![7_u8; 4096];

    // Altered on disk: the content no longer matches its hash.
    let shot = store.put(binary(&image), now()).await.unwrap();
    let shot_path = store.blob_path(&shot.content_hash, shot.encoding);
    std::fs::write(&shot_path, vec![8_u8; 4096]).unwrap();
    let error = store.read(shot.id).await.unwrap_err();
    assert!(
        matches!(error, ArtifactError::Corrupt { id } if id == shot.id),
        "{error}"
    );

    // A gzip blob that is no longer gzip cannot be decoded.
    let dom = store.put(text(&page), now()).await.unwrap();
    let dom_path = store.blob_path(&dom.content_hash, dom.encoding);
    std::fs::write(&dom_path, b"not gzip at all").unwrap();
    assert!(matches!(
        store.read(dom.id).await,
        Err(ArtifactError::Io { .. })
    ));

    // Removed from disk: the row is still there, the bytes are not.
    std::fs::remove_file(&dom_path).unwrap();
    let error = store.read(dom.id).await.unwrap_err();
    assert!(
        matches!(&error, ArtifactError::Missing { id, path } if *id == dom.id && *path == dom_path),
        "{error}"
    );
    assert!(error.to_string().contains("has no blob"));
    assert_eq!(store.get(dom.id).await.unwrap(), Some(dom));
}

#[tokio::test]
async fn the_retention_job_deletes_expired_artifacts_and_unreferenced_blobs() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let start = now();
    let retention = Duration::days(i64::from(RETENTION_DAYS));
    let shared = html(300);
    let lonely = html(301);

    // Day 0: one page stored twice (two references, one blob) and another
    // page stored once. Day 5: the first page is stored a third time.
    let shared_a = store.put(text(&shared), start).await.unwrap();
    let shared_b = store.put(text(&shared), start).await.unwrap();
    let lonely_a = store.put(text(&lonely), start).await.unwrap();
    let shared_late = store
        .put(text(&shared), start + Duration::days(5))
        .await
        .unwrap();
    let shared_path = store.blob_path(&shared_a.content_hash, shared_a.encoding);
    let lonely_path = store.blob_path(&lonely_a.content_hash, lonely_a.encoding);
    let lonely_size = lonely_a.stored_bytes;

    // Just before the deadline nothing expires.
    let early = store
        .sweep(start + retention - Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(early, SweepReport::default());
    assert!(shared_path.exists() && lonely_path.exists());

    // At the deadline the three day-0 references go. The lonely blob goes
    // with its last reference; the shared blob stays for the day-5 one.
    let report = store.sweep(start + retention).await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            artifacts_deleted: 3,
            blobs_deleted: 1,
            bytes_freed: lonely_size,
        }
    );
    assert!(!lonely_path.exists());
    assert!(shared_path.exists());
    for gone in [shared_a.id, shared_b.id, lonely_a.id] {
        assert_eq!(store.get(gone).await.unwrap(), None);
    }
    let (_, content) = store.read(shared_late.id).await.unwrap().unwrap();
    assert_eq!(content, shared);

    // Running the job again at the same time changes nothing.
    assert_eq!(
        store.sweep(start + retention).await.unwrap(),
        SweepReport::default()
    );

    // Five days later the last reference expires and the blob goes too.
    let last = store
        .sweep(start + retention + Duration::days(5))
        .await
        .unwrap();
    assert_eq!(last.artifacts_deleted, 1);
    assert_eq!(last.blobs_deleted, 1);
    assert_eq!(last.bytes_freed, shared_late.stored_bytes);
    assert_eq!(files_under(store.root()), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn the_retention_job_survives_a_blob_that_is_already_gone() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let start = now();
    let artifact = store.put(binary(&[1, 2, 3]), start).await.unwrap();
    std::fs::remove_file(store.blob_path(&artifact.content_hash, artifact.encoding)).unwrap();

    let report = store.sweep(start + Duration::days(30)).await.unwrap();

    // The row is deleted; there was no blob left to count.
    assert_eq!(
        report,
        SweepReport {
            artifacts_deleted: 1,
            blobs_deleted: 0,
            bytes_freed: 0,
        }
    );
    assert_eq!(store.get(artifact.id).await.unwrap(), None);
}

#[tokio::test]
async fn the_retention_job_works_through_more_rows_than_one_batch() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);
    let start = now();
    // More rows than one DELETE batch takes, sharing three blobs.
    let blobs = [vec![1_u8; 64], vec![2_u8; 64], vec![3_u8; 64]];
    for n in 0..620_usize {
        store.put(binary(&blobs[n % 3]), start).await.unwrap();
    }
    assert_eq!(files_under(store.root()).len(), 3);

    let report = store.sweep(start + Duration::days(15)).await.unwrap();

    assert_eq!(
        report,
        SweepReport {
            artifacts_deleted: 620,
            blobs_deleted: 3,
            bytes_freed: 192,
        }
    );
    assert_eq!(files_under(store.root()), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn orphaned_blobs_and_stale_temp_files_are_collected() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    let store = artifact_store(&db, &dir);

    // Before anything was stored there is no directory to scan.
    assert_eq!(
        store
            .sweep_orphans(now() + Duration::hours(1))
            .await
            .unwrap(),
        SweepReport::default()
    );

    let kept = store.put(binary(&[1; 100]), now()).await.unwrap();
    let kept_path = store.blob_path(&kept.content_hash, kept.encoding);
    let directory = kept_path.parent().unwrap();

    // A blob whose row was never committed (a crash between the two), in
    // both encodings.
    let orphan_hash = "ab".repeat(32);
    let orphan_dir = store.root().join("ab").join("ab");
    std::fs::create_dir_all(&orphan_dir).unwrap();
    let orphan = orphan_dir.join(&orphan_hash);
    let orphan_gz = orphan_dir.join(format!("{orphan_hash}.gz"));
    std::fs::write(&orphan, [9; 40]).unwrap();
    std::fs::write(&orphan_gz, [9; 10]).unwrap();
    // A write that never finished.
    let stale_temp = directory.join(format!(".{}.0000.tmp", kept.content_hash));
    std::fs::write(&stale_temp, [9; 5]).unwrap();
    // Files that are not the store's are never touched.
    let foreign = orphan_dir.join("README.txt");
    std::fs::write(&foreign, b"not ours").unwrap();
    let foreign_top = store.root().join("notes.txt");
    std::fs::write(&foreign_top, b"not ours").unwrap();
    tokio::time::sleep(StdDuration::from_millis(50)).await;

    // A cutoff before the files were written leaves writes in progress alone.
    let too_recent = store
        .sweep_orphans(now() - Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(too_recent, SweepReport::default());
    assert!(orphan.exists() && orphan_gz.exists() && stale_temp.exists());

    let report = store
        .sweep_orphans(now() + Duration::hours(1))
        .await
        .unwrap();

    assert_eq!(
        report,
        SweepReport {
            artifacts_deleted: 0,
            blobs_deleted: 3,
            bytes_freed: 55,
        }
    );
    assert!(!orphan.exists() && !orphan_gz.exists() && !stale_temp.exists());
    // Referenced blobs and foreign files stay.
    assert!(kept_path.exists() && foreign.exists() && foreign_top.exists());
    let (_, content) = store.read(kept.id).await.unwrap().unwrap();
    assert_eq!(content, vec![1; 100]);
}

#[tokio::test]
async fn a_store_that_cannot_write_reports_the_path() {
    let db = TestDb::migrated().await;
    let dir = TempDir::new().unwrap();
    // The artifact directory is a file: nothing can be created under it.
    let blocker = dir.path().join("artifacts");
    std::fs::write(&blocker, b"in the way").unwrap();
    let store = artifact_store(&db, &dir);

    let error = store.put(binary(&[1, 2, 3]), now()).await.unwrap_err();

    assert!(matches!(error, ArtifactError::Io { .. }), "{error}");
    assert!(error.to_string().contains("artifacts"));
    // No row was written for content that is not on disk.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
