//! Integration tests for in-core: the storage boundary driven through the
//! Turso implementation, and the provisioning and drive flows on top of it.
//!
//! New integration tests belong in this file rather than a new `tests/*.rs`:
//! one test binary links and runs once.

use std::path::PathBuf;

use in_core::store::{
    CHUNK_SIZE, ShareKind, Store, StoreError, ThumbState, TursoStore, UploadState,
};
use in_core::{File, Folder, User};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// A throwaway database on disk. Turso's in-memory mode is not what production
/// runs, so the tests exercise a real file.
struct Scratch {
    dir: PathBuf,
    db: PathBuf,
    storage: PathBuf,
    store: TursoStore,
}

impl Scratch {
    async fn open() -> Self {
        let dir = std::env::temp_dir().join(format!("in-test-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("in.db");
        let storage = dir.join("storage");
        let store = TursoStore::open(db.to_str().unwrap(), Some(&storage))
            .await
            .unwrap();
        Self {
            dir,
            db,
            storage,
            store,
        }
    }

    /// Reopens the same database file, then runs the hygiene passes the
    /// server runs in the background after `open` — the upload prune and
    /// the orphan sweep — against what the test left behind.
    async fn reopen(&mut self) {
        drop(std::mem::replace(
            &mut self.store,
            TursoStore::open(self.db.to_str().unwrap(), Some(&self.storage.clone()))
                .await
                .unwrap(),
        ));
        self.store.boot_sweeps().await.unwrap();
    }
}

/// A second, independent connection to the scratch database, for tests that
/// need to write a column the way it actually sits on disk rather than
/// through [`Store`]'s API.
async fn raw_conn(scratch: &Scratch) -> turso::Connection {
    let db = turso::Builder::new_local(scratch.db.to_str().unwrap())
        .build()
        .await
        .unwrap();
    db.connect().unwrap()
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn alice(store: &TursoStore) -> User {
    store.provision_user("sub-alice", "alice@example.com", "Alice", None, 0, 1024 * 1024 * 1024)
        .await
        .unwrap()
}

async fn bob(store: &TursoStore) -> User {
    store.provision_user("sub-bob", "bob@example.com", "Bob", None, 0, 1024 * 1024 * 1024)
        .await
        .unwrap()
}

/// A 4x4 red PNG: the smallest thing the thumbnailer must accept.
fn png_bytes() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(4, 4, image::Rgb([200, 30, 30]));
    let mut out = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut out),
        image::ImageFormat::Png,
    )
    .unwrap();
    out
}

#[tokio::test]
async fn the_schema_is_created_once_and_survives_reopen() {
    let dir = std::env::temp_dir().join(format!("in-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("in.db").to_string_lossy().into_owned();

    let first = TursoStore::open(&path, None).await.unwrap();
    alice(&first).await;
    drop(first);

    // Re-opening a database that already has tables must not re-run the
    // schema (its CREATE TABLE would fail on the first one) and must not
    // lose what the first open wrote. `None` storage derives beside the
    // database, so the second open finds the same tree.
    let second = TursoStore::open(&path, None).await.unwrap();
    assert_eq!(
        second
            .user_by_oidc_sub("sub-alice")
            .await
            .unwrap()
            .unwrap()
            .email,
        "alice@example.com"
    );
    assert!(dir.join("storage").join("files").is_dir());
    drop(second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_first_user_is_admin_and_sign_ins_refresh_the_row() {
    let scratch = Scratch::open().await;
    let first = alice(&scratch.store).await;
    assert!(first.admin);
    assert_eq!(first.quota_bytes, 1024 * 1024 * 1024);
    assert_eq!(first.used_bytes, 0);
    assert!(first.last_seen_at.is_some());

    let second = bob(&scratch.store).await;
    assert!(!second.admin);

    // A returning person keeps their id, admin flag and quota; the provider's
    // new address and name win, and the sighting is stamped.
    let again = scratch
        .store.provision_user("sub-alice", "alice@new.com", "Alice New", None, 0, 1)
        .await
        .unwrap();
    assert_eq!(again.id, first.id);
    assert!(again.admin);
    assert_eq!(again.email, "alice@new.com");
    assert_eq!(again.display_name, "Alice New");
    assert_eq!(again.quota_bytes, 1024 * 1024 * 1024);
}

#[tokio::test]
async fn quota_and_disablement_round_trip() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.set_user_quota(&user.id, 42).await.unwrap();
    scratch
        .store
        .set_user_disabled(&user.id, true)
        .await
        .unwrap();
    let back = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(back.quota_bytes, 42);
    assert!(back.disabled);
    assert!(matches!(
        scratch.store.set_user_quota("no-such-user", 1).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn folders_postfix_onto_live_sibling_names() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let root = scratch
        .store
        .create_folder(&user.id, None, "root")
        .await
        .unwrap();
    let kid = scratch
        .store
        .create_folder(&user.id, Some(&root.id), "kid")
        .await
        .unwrap();
    assert_eq!(kid.parent_id.as_deref(), Some(root.id.as_str()));

    // A live sibling wearing the name is no refusal: the second folder
    // takes the first free postfix, the next one the one after.
    let twin = scratch.store.create_folder(&user.id, None, "root").await.unwrap();
    assert_ne!(twin.id, root.id);
    assert_eq!(twin.name, "root (2)");
    let third = scratch.store.create_folder(&user.id, None, "root").await.unwrap();
    assert_eq!(third.name, "root (3)");
    // Same name in a different directory is a different name, as before.
    let away = scratch
        .store
        .create_folder(&user.id, Some(&root.id), "root")
        .await
        .unwrap();
    assert_eq!(away.name, "root");

    let listing = scratch.store.list_children(&user.id, None).await.unwrap();
    assert_eq!(listing.folders.len(), 3);
    let listing = scratch
        .store
        .list_children(&user.id, Some(&root.id))
        .await
        .unwrap();
    assert_eq!(listing.folders.len(), 2);
}

#[tokio::test]
async fn a_folder_moved_into_its_descendant_is_refused() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let a = scratch.store.create_folder(&user.id, None, "a").await.unwrap();
    let b = scratch
        .store
        .create_folder(&user.id, Some(&a.id), "b")
        .await
        .unwrap();
    let c = scratch
        .store
        .create_folder(&user.id, Some(&b.id), "c")
        .await
        .unwrap();

    assert!(matches!(
        scratch.store.move_folder(&a.id, Some(&c.id)).await,
        Err(StoreError::Cycle)
    ));
    assert!(matches!(
        scratch.store.move_folder(&a.id, Some(&a.id)).await,
        Err(StoreError::Cycle)
    ));
    // The refused moves changed nothing.
    let back: Folder = scratch.store.folder(&a.id).await.unwrap().unwrap();
    assert_eq!(back.parent_id, None);

    // A legal move lands.
    scratch.store.move_folder(&c.id, None).await.unwrap();
    let back: Folder = scratch.store.folder(&c.id).await.unwrap().unwrap();
    assert_eq!(back.parent_id, None);
}

#[tokio::test]
async fn strangers_folders_answer_cross_owner() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let other = bob(&scratch.store).await;
    let folder = scratch
        .store
        .create_folder(&user.id, None, "mine")
        .await
        .unwrap();

    // The store names the mismatch; the route layer answers 404 to it, never
    // 403, so a stranger learns nothing about which ids exist.
    assert!(matches!(
        scratch
            .store
            .create_folder(&other.id, Some(&folder.id), "squat")
            .await,
        Err(StoreError::CrossOwner)
    ));
    let file = scratch
        .store
        .insert_file(&user.id, None, "note", b"data")
        .await
        .unwrap();
    let bobs_folder = scratch
        .store
        .create_folder(&other.id, None, "bobs")
        .await
        .unwrap();
    assert!(matches!(
        scratch.store.move_file(&file.id, Some(&bobs_folder.id)).await,
        Err(StoreError::CrossOwner)
    ));
}
#[tokio::test]
async fn moving_across_owners_is_refused() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let other = bob(&scratch.store).await;
    let folder = scratch
        .store
        .create_folder(&user.id, None, "mine")
        .await
        .unwrap();
    let theirs = scratch
        .store
        .create_folder(&other.id, None, "theirs")
        .await
        .unwrap();
    assert!(matches!(
        scratch.store.move_folder(&folder.id, Some(&theirs.id)).await,
        Err(StoreError::CrossOwner)
    ));
}

#[tokio::test]
async fn names_are_labels_never_paths() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let folder = scratch
        .store
        .create_folder(&user.id, None, "../../etc")
        .await
        .unwrap();
    assert_eq!(folder.name, "etc");
    let file = scratch
        .store
        .insert_file(&user.id, None, "", b"data")
        .await
        .unwrap();
    assert_eq!(file.name, "Untitled");
}

#[tokio::test]
async fn an_inserted_file_round_trips_through_its_row_and_bytes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let mut rx = scratch.store.subscribe();
    let bytes = png_bytes();
    let file: File = scratch
        .store
        .insert_file(&user.id, None, "photo.png", &bytes)
        .await
        .unwrap();
    // The mime is sniffed from the bytes, the thumbnail is attempted for an
    // image, and the bytes land on disk under the row's id.
    assert_eq!(file.mime, "image/png");
    assert_eq!(file.thumb_state, ThumbState::Ready);
    assert_eq!(file.size_bytes, bytes.len() as u64);
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), bytes);
    assert!(scratch.storage.join("files").join(&file.id).is_file());
    let thumb = scratch.store.thumb_bytes(&file.id).await.unwrap().unwrap();
    assert_eq!(&thumb[0..4], b"RIFF");
    // The committed write announced the library.
    let change = rx.try_recv().unwrap();
    assert_eq!(change.topic.kind(), "library");

    // What the sniffer cannot name stays generic, and no thumbnail is
    // attempted for it.
    let text = scratch
        .store
        .insert_file(&user.id, None, "notes.txt", b"hello")
        .await
        .unwrap();
    assert_eq!(text.mime, "text/plain");
    assert_eq!(text.thumb_state, ThumbState::None);
    assert!(scratch.store.thumb_bytes(&text.id).await.unwrap().is_none());
}

/// Renders a tiny mp4 with `ffmpeg`'s own test pattern. `None` when there is
/// no `ffmpeg` on `PATH`, or the render itself fails — callers skip silently.
fn make_test_mp4() -> Option<Vec<u8>> {
    if !in_core::thumbs::ffmpeg_available() {
        return None;
    }
    let path = std::env::temp_dir().join(format!("in-test-upload-{}.mp4", Ulid::new()));
    let rendered = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=64x64:rate=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg4",
            "-y",
        ])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    let bytes = if rendered.success() {
        std::fs::read(&path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&path);
    bytes
}

#[tokio::test]
async fn a_broken_video_uploads_fine_without_a_thumbnail() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    // Wears an mp4's magic but holds garbage: `ffmpeg` — when present —
    // cannot frame it, and the upload must not care.
    let mut bytes = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00".to_vec();
    bytes.extend_from_slice(&[0xA5u8; 4096]);
    let file = scratch
        .store
        .insert_file(&user.id, None, "clip.mp4", &bytes)
        .await
        .unwrap();
    assert_eq!(file.mime, "video/mp4");
    // `Failed` when `ffmpeg` tried and missed, `None` when no attempt was
    // possible: never `Ready`, and never an upload error.
    assert!(matches!(
        file.thumb_state,
        ThumbState::Failed | ThumbState::None
    ));
    assert!(scratch.store.thumb_bytes(&file.id).await.unwrap().is_none());
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), bytes);
}

#[tokio::test]
async fn a_real_video_uploads_with_a_thumbnail() {
    let Some(bytes) = make_test_mp4() else {
        // No decoder on PATH, no test: the upload degrades to its icon.
        return;
    };
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let file = scratch
        .store
        .insert_file(&user.id, None, "clip.mp4", &bytes)
        .await
        .unwrap();
    assert_eq!(file.mime, "video/mp4");
    assert_eq!(file.thumb_state, ThumbState::Ready);
    let thumb = scratch.store.thumb_bytes(&file.id).await.unwrap().unwrap();
    assert_eq!(&thumb[0..4], b"RIFF");
    assert_eq!(&thumb[8..12], b"WEBP");
}

#[tokio::test]
async fn a_chunked_video_upload_finishes_with_a_thumbnail() {
    let Some(bytes) = make_test_mp4() else {
        return;
    };
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "clip.mp4", bytes.len() as u64)
        .await
        .unwrap();
    scratch.store.record_chunk(&session.id, 0, &bytes).await.unwrap();
    let file = scratch.store.finish_upload(&session.id).await.unwrap();
    assert_eq!(file.mime, "video/mp4");
    assert_eq!(file.thumb_state, ThumbState::Ready);
    let thumb = scratch.store.thumb_bytes(&file.id).await.unwrap().unwrap();
    assert_eq!(&thumb[0..4], b"RIFF");
    assert_eq!(&thumb[8..12], b"WEBP");
}

#[tokio::test]
async fn a_claimed_mime_is_never_trusted() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    // Named like an executable, holding a PNG: the row says image, because
    // the bytes do.
    let file = scratch
        .store
        .insert_file(&user.id, None, "evil.exe", &png_bytes())
        .await
        .unwrap();
    assert_eq!(file.mime, "image/png");
}

#[tokio::test]
async fn trashing_a_folder_cascades_and_restore_needs_live_ancestors() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let root = scratch.store.create_folder(&user.id, None, "root").await.unwrap();
    let kid = scratch
        .store
        .create_folder(&user.id, Some(&root.id), "kid")
        .await
        .unwrap();
    let file = scratch
        .store
        .insert_file(&user.id, Some(&kid.id), "deep.txt", b"deep")
        .await
        .unwrap();

    scratch.store.delete_folder(&root.id).await.unwrap();
    assert!(scratch.store.folder(&root.id).await.unwrap().unwrap().deleted_at.is_some());
    assert!(scratch.store.folder(&kid.id).await.unwrap().unwrap().deleted_at.is_some());
    assert!(scratch.store.file(&file.id).await.unwrap().unwrap().deleted_at.is_some());
    // Trashed rows leave the listing but join the trash.
    assert!(scratch.store.list_children(&user.id, None).await.unwrap().folders.is_empty());
    let trash = scratch.store.list_trash(&user.id).await.unwrap();
    assert_eq!(trash.folders.len(), 2);
    assert_eq!(trash.files.len(), 1);

    // Restoring from under trash is refused: the ancestor is still down.
    assert!(matches!(
        scratch.store.restore_folder(&kid.id).await,
        Err(StoreError::AncestorTrashed)
    ));
    assert!(matches!(
        scratch.store.restore_file(&file.id).await,
        Err(StoreError::AncestorTrashed)
    ));

    // Restoring the root brings the whole subtree back.
    scratch.store.restore_folder(&root.id).await.unwrap();
    assert!(scratch.store.folder(&kid.id).await.unwrap().unwrap().deleted_at.is_none());
    assert!(scratch.store.file(&file.id).await.unwrap().unwrap().deleted_at.is_none());
    assert_eq!(scratch.store.list_children(&user.id, None).await.unwrap().folders.len(), 1);
}

#[tokio::test]
async fn a_trashed_folder_name_restores_beside_a_live_twin() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let first = scratch.store.create_folder(&user.id, None, "x").await.unwrap();
    scratch.store.delete_folder(&first.id).await.unwrap();
    // Deleting frees the name, and restoring beside the squatter postfixes
    // instead of refusing or duplicating.
    scratch.store.create_folder(&user.id, None, "x").await.unwrap();
    scratch.store.restore_folder(&first.id).await.unwrap();
    let listing = scratch.store.list_children(&user.id, None).await.unwrap();
    assert_eq!(listing.folders.len(), 2);
    assert!(listing.folders.iter().any(|folder| folder.name == "x"));
    assert!(listing.folders.iter().any(|folder| folder.name == "x (2)"));
}

#[tokio::test]
async fn used_bytes_counts_trash_and_purge_frees_it() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let a = scratch.store.insert_file(&user.id, None, "a", &[0u8; 10]).await.unwrap();
    let b = scratch.store.insert_file(&user.id, None, "b", &[0u8; 20]).await.unwrap();
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 30);

    // Trash counts: the bytes are still on disk, still restorable.
    scratch.store.delete_file(&a.id).await.unwrap();
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 30);
    scratch.store.restore_file(&a.id).await.unwrap();
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 30);

    // Purge frees: the row goes and the bytes follow.
    scratch.store.delete_file(&a.id).await.unwrap();
    assert!(scratch.store.purge_file(&a.id).await.unwrap());
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 20);
    assert!(!scratch.storage.join("files").join(&a.id).exists());

    // A live file is never purged, and a missing one is not found.
    assert!(!scratch.store.purge_file(&b.id).await.unwrap());
    assert!(matches!(
        scratch.store.purge_file("no-such-file").await,
        Err(StoreError::NotFound)
    ));

    // Emptying the trash takes the rest.
    scratch.store.delete_file(&b.id).await.unwrap();
    assert_eq!(scratch.store.empty_trash(&user.id).await.unwrap(), 1);
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 0);
    assert!(scratch.store.list_trash(&user.id).await.unwrap().files.is_empty());
}

#[tokio::test]
async fn quota_is_enforced_at_start_and_at_finish() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.set_user_quota(&user.id, 100).await.unwrap();

    // Over quota at open: refused with nothing on disk to clean up.
    assert!(matches!(
        scratch.store.create_upload_session(&user.id, None, "big", 101).await,
        Err(StoreError::QuotaExceeded)
    ));

    // Fits at open, over quota by finish: the chunks stay staged for another
    // attempt once space is freed.
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "late", 90)
        .await
        .unwrap();
    scratch.store.record_chunk(&session.id, 0, &[7u8; 90]).await.unwrap();
    scratch.store.insert_file(&user.id, None, "filler", &[0u8; 20]).await.unwrap();
    assert!(matches!(
        scratch.store.finish_upload(&session.id).await,
        Err(StoreError::QuotaExceeded)
    ));
    let back = scratch.store.upload_session(&session.id).await.unwrap().unwrap();
    assert_eq!(back.state, UploadState::Active);

    // Room freed, the same chunks finish.
    scratch.store.set_user_quota(&user.id, 200).await.unwrap();
    let file = scratch.store.finish_upload(&session.id).await.unwrap();
    assert_eq!(file.size_bytes, 90);
}

#[tokio::test]
async fn trash_writes_are_invisible_and_purge_takes_subtrees() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let root = scratch.store.create_folder(&user.id, None, "root").await.unwrap();
    scratch.store.delete_folder(&root.id).await.unwrap();

    // Nothing is created, moved or uploaded under trash.
    assert!(matches!(
        scratch.store.create_folder(&user.id, Some(&root.id), "kid").await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        scratch.store.insert_file(&user.id, Some(&root.id), "f", b"x").await,
        Err(StoreError::NotFound)
    ));

    // Purging the expired takes the folder row itself.
    let purged = scratch
        .store
        .purge_expired(OffsetDateTime::now_utc() + Duration::days(31))
        .await
        .unwrap();
    assert_eq!(purged, 1);
    assert!(scratch.store.folder(&root.id).await.unwrap().is_none());
    // Too fresh to purge: a cutoff taken before the trash moment matches
    // nothing.
    let root2 = scratch.store.create_folder(&user.id, None, "root2").await.unwrap();
    let cutoff = OffsetDateTime::now_utc();
    scratch.store.delete_folder(&root2.id).await.unwrap();
    assert_eq!(scratch.store.purge_expired(cutoff).await.unwrap(), 0);
}
#[tokio::test]
async fn share_links_open_until_revoked_or_expired() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let file = scratch.store.insert_file(&user.id, None, "f", b"data").await.unwrap();
    let now = OffsetDateTime::now_utc();

    let created = scratch
        .store
        .create_share_link(&user.id, ShareKind::File, &file.id, true, None, None)
        .await
        .unwrap();
    // The token is shown once; the row keeps only the hash.
    assert!(!created.token.is_empty());
    assert_ne!(created.link.token_hash, created.token);
    // The route hashes the plaintext with the store's own function.
    assert_eq!(in_core::hash_share_token(&created.token), created.link.token_hash);
    let resolved = scratch
        .store
        .resolve_share_link(&created.link.token_hash, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.target_id, file.id);
    assert!(resolved.can_download);

    // A wrong token and a revoked link both answer None.
    assert!(scratch.store.resolve_share_link("wrong", now).await.unwrap().is_none());
    scratch.store.revoke_share_link(&created.link.id).await.unwrap();
    assert!(scratch.store.resolve_share_link(&created.link.token_hash, now).await.unwrap().is_none());
    // Revoking twice is not an error.
    scratch.store.revoke_share_link(&created.link.id).await.unwrap();

    // An already-expired link never opens.
    let past = scratch
        .store
        .create_share_link(
            &user.id,
            ShareKind::File,
            &file.id,
            false,
            Some(now - Duration::hours(1)),
            None,
        )
        .await
        .unwrap();
    assert!(!past.link.is_live(now));
    assert!(scratch.store.resolve_share_link(&past.link.token_hash, now).await.unwrap().is_none());
    assert_eq!(scratch.store.share_links(&user.id).await.unwrap().len(), 2);
}

/// The drive page's Shared mark reads its set from here: live links and
/// given grants are in, revoked and expired links are out, and another
/// owner's grants never leak in.
#[tokio::test]
async fn shared_target_ids_holds_live_links_and_own_grants() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let linked = scratch.store.insert_file(&user.id, None, "linked", b"d").await.unwrap();
    let granted = scratch.store.insert_file(&user.id, None, "granted", b"d").await.unwrap();
    let folder = scratch.store.create_folder(&user.id, None, "dossier").await.unwrap();
    let strangers = scratch.store.insert_file(&friend.id, None, "theirs", b"d").await.unwrap();

    // Empty while nothing is shared.
    assert!(scratch.store.shared_target_ids(&user.id).await.unwrap().is_empty());

    let file_link = scratch
        .store
        .create_share_link(&user.id, ShareKind::File, &linked.id, true, None, None)
        .await
        .unwrap();
    scratch
        .store
        .add_share_user(&user.id, ShareKind::File, &granted.id, &friend.id, false)
        .await
        .unwrap();
    scratch
        .store
        .add_share_user(&user.id, ShareKind::Folder, &folder.id, &friend.id, true)
        .await
        .unwrap();

    let shared: std::collections::HashSet<_> = scratch
        .store
        .shared_target_ids(&user.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    let expected: std::collections::HashSet<_> = vec![
        (ShareKind::File, granted.id.clone()),
        (ShareKind::File, linked.id.clone()),
        (ShareKind::Folder, folder.id.clone()),
    ]
    .into_iter()
    .collect();
    assert_eq!(shared, expected);
    // The friend shares nothing of their own: a grant received is not a
    // grant given.
    assert!(scratch.store.shared_target_ids(&friend.id).await.unwrap().is_empty());

    // A revoked link drops out; the grant stays.
    scratch.store.revoke_share_link(&file_link.link.id).await.unwrap();
    let shared = scratch.store.shared_target_ids(&user.id).await.unwrap();
    assert_eq!(shared.len(), 2);
    assert!(!shared.contains(&(ShareKind::File, linked.id.clone())));

    // An expired link drops out too.
    let past = scratch
        .store
        .create_share_link(
            &user.id,
            ShareKind::File,
            &linked.id,
            true,
            Some(OffsetDateTime::now_utc() - Duration::hours(1)),
            None,
        )
        .await
        .unwrap();
    let shared = scratch.store.shared_target_ids(&user.id).await.unwrap();
    assert!(!shared.contains(&(ShareKind::File, past.link.target_id.clone())));
    assert!(!shared.contains(&(ShareKind::File, strangers.id.clone())));

    // Deleting the grant removes the last mark its target had.
    scratch
        .store
        .remove_share_user(ShareKind::File, &granted.id, &friend.id)
        .await
        .unwrap();
    let shared = scratch.store.shared_target_ids(&user.id).await.unwrap();
    assert_eq!(
        shared,
        vec![(ShareKind::Folder, folder.id.clone())],
        "the folder link is all that is left"
    );
}

#[tokio::test]
async fn a_password_link_round_trips_its_hash() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let file = scratch
        .store
        .insert_file(&user.id, None, "locked", b"data")
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();

    let hash = in_core::hash_link_password("open sesame").unwrap();
    let created = scratch
        .store
        .create_share_link(
            &user.id,
            ShareKind::File,
            &file.id,
            true,
            None,
            Some(hash.clone()),
        )
        .await
        .unwrap();

    // The hash is really on disk: a fresh database carries the column, and
    // the row reads back through a second connection the way it sits.
    let raw = raw_conn(&scratch).await;
    let mut rows = raw
        .query(
            "SELECT password_hash FROM share_link WHERE id = ?1",
            turso::params![created.link.id.as_str()],
        )
        .await
        .unwrap();
    let stored = rows.next().await.unwrap().unwrap();
    assert_eq!(stored.get::<String>(0).unwrap(), hash);

    // Resolving hands the hash back, so the route can mint the proof
    // without a second read.
    let resolved = scratch
        .store
        .resolve_share_link(&created.link.token_hash, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.password_hash.as_deref(), Some(hash.as_str()));

    // The check answers the right password and refuses the rest — and a
    // malformed hash refuses without panicking, the way a gate must.
    assert!(in_core::link_password_matches(&hash, "open sesame"));
    assert!(!in_core::link_password_matches(&hash, "Open Sesame"));
    assert!(!in_core::link_password_matches(&hash, ""));
    assert!(!in_core::link_password_matches(
        "not a phc string",
        "open sesame"
    ));

    // The proof binds the token to the stored hash: same pair, same proof;
    // any other pair — another link's, or a re-keyed hash — answers
    // differently.
    assert_eq!(
        in_core::link_unlock_proof(&created.token, &hash),
        in_core::link_unlock_proof(&created.token, &hash)
    );
    assert_ne!(
        in_core::link_unlock_proof(&created.token, &hash),
        in_core::link_unlock_proof("another-token", &hash)
    );
}
#[tokio::test]
async fn a_pre_password_link_reconciles_to_no_password_and_a_new_one_survives_reopen() {
    // A database built from 0001+0004 — the shape in ran with before links
    // could carry passwords — has a share_link table with no
    // `password_hash` column. Opening it with the current code reconciles
    // it onto the declared schema: the link row is carried across and wears
    // NULL, opening like it always did, and a password minted after the
    // rebuild survives the next boot whole.
    let dir = std::env::temp_dir().join(format!("in-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("in.db").to_string_lossy().into_owned();
    {
        let db = turso::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))
            .await
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0002_ui.sql"))
            .await
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0003_preferences.sql"))
            .await
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0004_downloads_settings.sql"))
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO user (id, oidc_sub, email, display_name, admin, disabled, \
             quota_bytes, used_bytes, created_at, last_seen_at) \
             VALUES ('u-old', 'sub-old', 'old@example.com', 'Old', 1, 0, 100, 0, \
             '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO file (id, owner_id, folder_id, name, mime, size_bytes, \
             thumb_state, created_at, updated_at, deleted_at) \
             VALUES ('f-old', 'u-old', NULL, 'old.txt', 'text/plain', 3, 'none', \
             '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z', NULL)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO share_link (id, token_hash, kind, target_id, created_by, \
             can_download, created_at, expires_at, revoked_at) \
             VALUES ('l-old', 'hash-old', 'file', 'f-old', 'u-old', 1, \
             '2026-08-01T00:00:00Z', NULL, NULL)",
            (),
        )
        .await
        .unwrap();
    }
    let storage = dir.join("storage");
    let store = TursoStore::open(&path, Some(&storage)).await.unwrap();
    let links = store.share_links("u-old").await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].id, "l-old");
    assert_eq!(links[0].password_hash, None);

    // A password minted after the rebuild is an ordinary row: the next open
    // sees the declared shape, changes nothing, and the hash survives.
    let hash = in_core::hash_link_password("open sesame").unwrap();
    store
        .create_share_link(
            "u-old",
            ShareKind::File,
            "f-old",
            true,
            None,
            Some(hash.clone()),
        )
        .await
        .unwrap();
    drop(store);
    let store = TursoStore::open(&path, Some(&storage)).await.unwrap();
    let links = store.share_links("u-old").await.unwrap();
    assert_eq!(links.len(), 2);
    let carried = links.iter().find(|link| link.id != "l-old").unwrap();
    assert_eq!(carried.password_hash.as_deref(), Some(hash.as_str()));
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
#[tokio::test]
async fn per_person_shares_gate_visibility() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let stranger = scratch
        .store.provision_user("sub-caro", "caro@example.com", "Caro", None, 0, 1024)
        .await
        .unwrap();
    let file = scratch.store.insert_file(&user.id, None, "shared", b"data").await.unwrap();

    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, true).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    // The owner always sees; a stranger never does; a missing target is
    // simply not seen.
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &user.id).await.unwrap());
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &stranger.id).await.unwrap());
    assert!(!scratch.store.can_see(ShareKind::File, "missing", &friend.id).await.unwrap());

    let shared = scratch.store.shares_for_user(&friend.id).await.unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].name, "shared");
    assert_eq!(shared[0].mime.as_deref(), Some("text/plain"));

    // Trash hides the target from the grantee but not from the owner; purge
    // takes the grant with the row.
    scratch.store.delete_file(&file.id).await.unwrap();
    assert!(scratch.store.shares_for_user(&friend.id).await.unwrap().is_empty());
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &user.id).await.unwrap());

    scratch.store.remove_share_user(ShareKind::File, &file.id, &friend.id).await.unwrap();
    scratch.store.restore_file(&file.id).await.unwrap();
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
}

#[tokio::test]
async fn an_upload_assembles_chunks_in_order_and_sniffs() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let bytes = png_bytes();
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "avatar.exe", bytes.len() as u64)
        .await
        .unwrap();
    assert_eq!(session.chunk_size, CHUNK_SIZE);
    // A chunk that is not exactly the promised bytes is refused.
    assert!(matches!(
        scratch.store.record_chunk(&session.id, 0, b"short").await,
        Err(StoreError::BadChunk)
    ));
    assert!(matches!(
        scratch.store.record_chunk(&session.id, 1, &bytes).await,
        Err(StoreError::BadChunk)
    ));
    let back = scratch.store.record_chunk(&session.id, 0, &bytes).await.unwrap();
    assert_eq!(back.received_bytes, bytes.len() as u64);

    let file = scratch.store.finish_upload(&session.id).await.unwrap();
    // Named like an executable, holding a PNG: sniffed, thumbed, stored.
    assert_eq!(file.name, "avatar.exe");
    assert_eq!(file.mime, "image/png");
    assert_eq!(file.thumb_state, ThumbState::Ready);
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), bytes);
    assert!(scratch.storage.join("files").join(&file.id).is_file());
    let done = scratch.store.upload_session(&session.id).await.unwrap().unwrap();
    assert_eq!(done.state, UploadState::Done);
    assert!(!scratch.storage.join("uploads").join(&session.id).exists());
}

#[tokio::test]
async fn an_empty_upload_finishes_an_empty_file() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch.store.create_upload_session(&user.id, None, "empty", 0).await.unwrap();
    let file = scratch.store.finish_upload(&session.id).await.unwrap();
    assert_eq!(file.size_bytes, 0);
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), Vec::<u8>::new());
}

#[tokio::test]
async fn uploaded_chunks_names_what_a_resume_may_skip() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "resume.bin", CHUNK_SIZE + 4)
        .await
        .unwrap();
    assert_eq!(
        scratch.store.uploaded_chunks(&session.id).await.unwrap(),
        Vec::<u64>::new()
    );
    // Out of order on purpose: the answer is read off the chunk files on
    // disk and comes back sorted, whatever order the calls arrived in.
    scratch.store.record_chunk(&session.id, 1, b"tail").await.unwrap();
    assert_eq!(scratch.store.uploaded_chunks(&session.id).await.unwrap(), vec![1]);
    scratch
        .store
        .record_chunk(&session.id, 0, &vec![7u8; CHUNK_SIZE as usize])
        .await
        .unwrap();
    assert_eq!(scratch.store.uploaded_chunks(&session.id).await.unwrap(), vec![0, 1]);
    assert!(matches!(
        scratch.store.uploaded_chunks("no-such-session").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn uploads_abort_and_prune() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "abandoned", 4)
        .await
        .unwrap();
    scratch.store.record_chunk(&session.id, 0, b"1234").await.unwrap();
    scratch.store.abort_upload(&session.id).await.unwrap();
    assert_eq!(
        scratch.store.upload_session(&session.id).await.unwrap().unwrap().state,
        UploadState::Aborted
    );
    assert!(!scratch.storage.join("uploads").join(&session.id).exists());
    // Chunks on a dead session are refused; aborting twice is fine.
    assert!(matches!(
        scratch.store.record_chunk(&session.id, 0, b"1234").await,
        Err(StoreError::UploadExpired)
    ));
    scratch.store.abort_upload(&session.id).await.unwrap();

    // Expiry prunes: the row says aborted and the chunks are gone.
    let session = scratch.store.create_upload_session(&user.id, None, "stale", 4).await.unwrap();
    scratch.store.record_chunk(&session.id, 0, b"1234").await.unwrap();
    let pruned = scratch
        .store
        .prune_expired_uploads(OffsetDateTime::now_utc() + Duration::hours(25))
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    assert!(!scratch.storage.join("uploads").join(&session.id).exists());
    // Nothing expired: nothing pruned.
    assert_eq!(
        scratch.store.prune_expired_uploads(OffsetDateTime::now_utc()).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn search_finds_live_names_and_nothing_else() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let other = bob(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "quarterly report", b"1").await.unwrap();
    scratch.store.insert_file(&user.id, None, "quarterly review", b"2").await.unwrap();
    scratch.store.create_folder(&user.id, None, "quarters").await.unwrap();
    let trashed = scratch.store.insert_file(&user.id, None, "quarterly old", b"3").await.unwrap();
    scratch.store.insert_file(&other.id, None, "quarterly theirs", b"4").await.unwrap();
    scratch.store.delete_file(&trashed.id).await.unwrap();

    let hits = scratch.store.search(&user.id, "quarter", 50).await.unwrap();
    assert_eq!(hits.files.len(), 2);
    assert_eq!(hits.folders.len(), 1);
    // Wildcards in the query search literally.
    let hits = scratch.store.search(&user.id, "report%", 50).await.unwrap();
    assert!(hits.files.is_empty());
    assert!(scratch.store.search(&user.id, "   ", 50).await.unwrap().files.is_empty());
}

/// Moves the database's watermark — the newest moment any row carries — to
/// `stamp`, so a test decides which side of it the planted junk falls on
/// rather than the clock. `insert_file` writes "now"; a far-future stamp
/// makes everything on disk older than the database, a far-past one makes
/// everything newer.
async fn restamp_files(scratch: &Scratch, stamp: &str) {
    let conn = raw_conn(scratch).await;
    conn.execute(
        "UPDATE file SET created_at = ?1, updated_at = ?1",
        turso::params![stamp],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_boot_sweep_deletes_files_no_row_names() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "kept", b"data").await.unwrap();
    let stray = scratch.storage.join("files").join("stray");
    std::fs::write(&stray, b"orphan").unwrap();
    let ghost = scratch.storage.join("uploads").join("ghost");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(ghost.join("0"), b"chunk").unwrap();
    // Both are older than what the database knows, which is the only case
    // in which the sweep may reclaim them.
    restamp_files(&scratch, "2099-01-01T00:00:00Z").await;
    scratch.reopen().await;
    assert!(!stray.exists());
    assert!(!ghost.exists());
}

/// The incident this rule exists for: a database three days behind the
/// shared bucket deleted four real uploads. An object newer than the newest
/// row is an upload the database has not heard of yet, never an orphan.
#[tokio::test]
async fn the_boot_sweep_keeps_what_is_newer_than_the_database() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "old", b"data").await.unwrap();
    // The database is behind the store: its rows are ancient, the objects
    // beside them arrived since.
    restamp_files(&scratch, "2000-01-01T00:00:00Z").await;
    let unknown = scratch.storage.join("files").join("uploaded-elsewhere");
    std::fs::write(&unknown, b"a real upload").unwrap();
    let staged = scratch.storage.join("uploads").join("live-session");
    std::fs::create_dir_all(&staged).unwrap();
    scratch.reopen().await;
    assert!(unknown.exists());
    assert!(staged.exists());
}

/// A fresh database against a populated bucket is the stale case at its
/// worst: it knows no moment at all, so it may reclaim nothing.
#[tokio::test]
async fn the_boot_sweep_keeps_everything_when_the_database_is_empty() {
    let mut scratch = Scratch::open().await;
    let stray = scratch.storage.join("files").join("stray");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"someone else's upload").unwrap();
    let ghost = scratch.storage.join("uploads").join("ghost");
    std::fs::create_dir_all(&ghost).unwrap();
    scratch.reopen().await;
    assert!(stray.exists());
    assert!(ghost.exists());
}

/// Thumbnails follow the files' rule, both ways.
#[tokio::test]
async fn the_boot_sweep_weighs_thumbnails_by_the_same_watermark() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "row", b"data").await.unwrap();
    let thumbs = scratch.storage.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let old = thumbs.join("stale-thumb");
    std::fs::write(&old, b"orphan").unwrap();
    restamp_files(&scratch, "2099-01-01T00:00:00Z").await;
    scratch.reopen().await;
    assert!(!old.exists());

    let new = thumbs.join("fresh-thumb");
    std::fs::write(&new, b"newer than the database").unwrap();
    restamp_files(&scratch, "2000-01-01T00:00:00Z").await;
    scratch.reopen().await;
    assert!(new.exists());
}

#[tokio::test]
async fn a_row_whose_file_is_missing_is_kept() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let file = scratch.store.insert_file(&user.id, None, "lost", b"data").await.unwrap();
    std::fs::remove_file(scratch.storage.join("files").join(&file.id)).unwrap();
    scratch.reopen().await;
    // Said out loud at boot, kept in the database: a lost file is a lost
    // file, not a lost fact.
    assert!(scratch.store.file(&file.id).await.unwrap().is_some());
    assert!(scratch.store.file_bytes(&file.id).await.unwrap().is_none());
}

#[tokio::test]
async fn the_boot_prune_aborts_expired_sessions() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch.store.create_upload_session(&user.id, None, "stale", 4).await.unwrap();
    scratch.store.record_chunk(&session.id, 0, b"1234").await.unwrap();
    // Backdate the expiry the way a day-old row would wear it.
    let conn = raw_conn(&scratch).await;
    conn.execute(
        "UPDATE upload_session SET expires_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
        turso::params![session.id.as_str()],
    )
    .await
    .unwrap();
    drop(conn);
    scratch.reopen().await;
    let back = scratch.store.upload_session(&session.id).await.unwrap().unwrap();
    assert_eq!(back.state, UploadState::Aborted);
    assert!(!scratch.storage.join("uploads").join(&session.id).exists());
}

/// The sweep runs beside live traffic now, and a writer's bytes land on
/// disk before its row commits — both inside one write lock. This pins the
/// lock's job: a writer mid-insert (its file already on disk, its row
/// uncommitted, its transaction holding the write lock) must survive a
/// sweep that starts while it works. The sweep waits for the writer's lock
/// instead of reading the tree in between, so the committed file and its
/// bytes both come out the other side — while a stray nothing names still
/// goes, proving the sweep ran at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_waits_for_a_writer_instead_of_deleting_its_file() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;

    // A writer the way `insert_file` is built: IMMEDIATE transaction
    // first, then bytes on disk, then the row — the lock held throughout.
    let db_path = scratch.db.clone();
    let file_path = scratch.storage.join("files").join("midwrite");
    let writer_path = file_path.clone();
    let (locked, locked_rx) = tokio::sync::oneshot::channel::<()>();
    let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let db = turso::Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
        std::fs::write(&writer_path, b"arriving").unwrap();
        conn.execute(
            "INSERT INTO file (id, owner_id, folder_id, name, mime, size_bytes, \
             thumb_state, created_at, updated_at, deleted_at, download_count) \
             VALUES ('midwrite', ?1, NULL, 'midwrite.txt', 'text/plain', 8, 'none', \
             '2099-01-01T00:00:00Z', '2099-01-01T00:00:00Z', NULL, 0)",
            turso::params![user.id.as_str()],
        )
        .await
        .unwrap();
        let _ = locked.send(());
        // Hold the write lock until the sweep is contending on it: only a
        // sweep that waits can then see the committed row.
        let _ = go_rx.await;
        conn.execute("COMMIT", ()).await.unwrap();
    });

    locked_rx.await.unwrap();
    std::fs::write(scratch.storage.join("files").join("stray"), b"orphan").unwrap();
    let sweep = tokio::spawn({
        let storage = scratch.storage.clone();
        async move {
            // The sweep contends here: its BEGIN IMMEDIATE cannot be
            // granted until the writer commits, and the sleep only makes
            // that contention certain rather than likely.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = go.send(());
            storage
        }
    });
    scratch.store.boot_sweeps().await.unwrap();
    writer.await.unwrap();
    let storage = sweep.await.unwrap();
    // The sweep ran — the stray is gone — and the writer's file survived,
    // row and bytes both.
    assert!(!storage.join("files").join("stray").exists());
    assert!(scratch.store.file("midwrite").await.unwrap().is_some());
    assert!(file_path.exists());
}
#[tokio::test]
async fn email_lookup_folds_case() {
    let scratch = Scratch::open().await;
    let user = scratch
        .store.provision_user("sub-mixed", "Mixed@Example.COM", "Mixed", None, 0, 1024)
        .await
        .unwrap();
    // Stored folded, found however it is asked.
    assert_eq!(user.email, "mixed@example.com");
    for query in ["mixed@example.com", "MIXED@EXAMPLE.COM", "  Mixed@Example.com  "] {
        let found = scratch.store.user_by_email(query).await.unwrap().unwrap();
        assert_eq!(found.id, user.id);
    }
    assert!(scratch.store.user_by_email("nobody@example.com").await.unwrap().is_none());
}

#[tokio::test]
async fn download_needs_the_flag() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let file = scratch.store.insert_file(&user.id, None, "f", b"data").await.unwrap();

    // The owner always downloads; a stranger never does.
    assert!(scratch.store.can_download(ShareKind::File, &file.id, &user.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, "missing", &friend.id).await.unwrap());

    // A view-only grant opens the page but not the bytes.
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, false).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());

    // Re-sharing with the flag flips the grant in place.
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, true).await.unwrap();
    assert!(scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());

    // Unsharing closes both.
    scratch.store.remove_share_user(ShareKind::File, &file.id, &friend.id).await.unwrap();
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());
}

#[tokio::test]
async fn a_folder_grant_covers_everything_under_it() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let parent = scratch.store.create_folder(&user.id, None, "parent").await.unwrap();
    let kid = scratch
        .store
        .create_folder(&user.id, Some(&parent.id), "kid")
        .await
        .unwrap();
    let file = scratch
        .store
        .insert_file(&user.id, Some(&kid.id), "deep.txt", b"deep")
        .await
        .unwrap();
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());

    // One grant on the top folder opens the whole subtree, files included.
    scratch.store.add_share_user(&user.id, ShareKind::Folder, &parent.id, &friend.id, true).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::Folder, &kid.id, &friend.id).await.unwrap());
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());

    // Revoking the one grant closes the whole subtree again.
    scratch.store.remove_share_user(ShareKind::Folder, &parent.id, &friend.id).await.unwrap();
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());

    // A view-only folder grant sees the subtree but downloads nothing in it.
    scratch.store.add_share_user(&user.id, ShareKind::Folder, &parent.id, &friend.id, false).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(!scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());
}

#[tokio::test]
async fn open_does_not_purge_trash() {
    let mut scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let folder = scratch.store.create_folder(&user.id, None, "doomed").await.unwrap();
    scratch.store.delete_folder(&folder.id).await.unwrap();
    // Age the trash past any deployment's cutoff the way a month-old row
    // would wear it.
    let conn = raw_conn(&scratch).await;
    conn.execute(
        "UPDATE folder SET deleted_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
        turso::params![folder.id.as_str()],
    )
    .await
    .unwrap();
    drop(conn);
    // Reopening prunes uploads and sweeps orphans, but the trash waits for
    // an explicit purge_expired with the deployment's own cutoff.
    scratch.reopen().await;
    assert!(scratch.store.folder(&folder.id).await.unwrap().unwrap().deleted_at.is_some());
    assert_eq!(
        scratch.store.purge_expired(OffsetDateTime::now_utc()).await.unwrap(),
        1
    );
    assert!(scratch.store.folder(&folder.id).await.unwrap().is_none());
}
#[tokio::test]
async fn purge_folder_removes_the_whole_trashed_subtree() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let root = scratch.store.create_folder(&user.id, None, "root").await.unwrap();
    let kid = scratch
        .store
        .create_folder(&user.id, Some(&root.id), "kid")
        .await
        .unwrap();
    let file = scratch
        .store
        .insert_file(&user.id, Some(&kid.id), "deep.txt", b"deep")
        .await
        .unwrap();
    // A live folder is never purged — trash it first — and neither is a
    // missing one.
    assert!(matches!(
        scratch.store.purge_folder(&root.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        scratch.store.purge_folder("no-such-folder").await,
        Err(StoreError::NotFound)
    ));

    scratch.store.delete_folder(&root.id).await.unwrap();
    // Two folders and one file go: rows, bytes, usage and shares together.
    assert_eq!(scratch.store.purge_folder(&root.id).await.unwrap(), 3);
    assert!(scratch.store.folder(&root.id).await.unwrap().is_none());
    assert!(scratch.store.folder(&kid.id).await.unwrap().is_none());
    assert!(scratch.store.file(&file.id).await.unwrap().is_none());
    assert!(!scratch.storage.join("files").join(&file.id).exists());
    assert_eq!(scratch.store.user(&user.id).await.unwrap().unwrap().used_bytes, 0);
    assert!(scratch.store.list_trash(&user.id).await.unwrap().folders.is_empty());
}

#[tokio::test]
async fn finish_upload_refused_leaves_no_bytes_behind() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let session = scratch.store.create_upload_session(&user.id, None, "late", 4).await.unwrap();
    scratch.store.record_chunk(&session.id, 0, b"1234").await.unwrap();
    // The library fills while the chunks arrive: someone else's bytes land
    // first and the ceiling drops under the session's total.
    scratch.store.insert_file(&user.id, None, "filler", b"12345").await.unwrap();
    scratch.store.set_user_quota(&user.id, 5).await.unwrap();
    let before = {
        let mut names: Vec<_> = std::fs::read_dir(scratch.storage.join("files"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    assert!(matches!(
        scratch.store.finish_upload(&session.id).await,
        Err(StoreError::QuotaExceeded)
    ));
    // Refused before the rename: files/ holds exactly what it held, with no
    // assembled copy and no temp left behind — and the chunks stay staged
    // for another attempt.
    let after = {
        let mut names: Vec<_> = std::fs::read_dir(scratch.storage.join("files"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    assert_eq!(before, after);
    assert!(scratch.storage.join("uploads").join(&session.id).is_dir());
}

#[tokio::test]
async fn a_download_grant_above_a_view_only_grant_still_downloads() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let folder = scratch.store.create_folder(&user.id, None, "shared").await.unwrap();
    let file = scratch.store.insert_file(&user.id, Some(&folder.id), "note", b"data").await.unwrap();
    // Most permissive wins across the chain: the folder's download grant
    // opens the bytes even with a view-only grant on the file itself.
    scratch.store.add_share_user(&user.id, ShareKind::Folder, &folder.id, &friend.id, true).await.unwrap();
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, false).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    assert!(scratch.store.can_download(ShareKind::File, &file.id, &friend.id).await.unwrap());
}

#[tokio::test]
async fn restore_folder_keeps_files_trashed_before_it() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let folder = scratch.store.create_folder(&user.id, None, "box").await.unwrap();
    let early = scratch.store.insert_file(&user.id, Some(&folder.id), "early", b"1").await.unwrap();
    let with = scratch.store.insert_file(&user.id, Some(&folder.id), "with", b"2").await.unwrap();
    // Trashed on its own first, then the folder takes the rest with it.
    scratch.store.delete_file(&early.id).await.unwrap();
    scratch.store.delete_folder(&folder.id).await.unwrap();
    scratch.store.restore_folder(&folder.id).await.unwrap();
    // The cascade comes back; the earlier trash stays trash.
    assert!(scratch.store.folder(&folder.id).await.unwrap().unwrap().deleted_at.is_none());
    assert!(scratch.store.file(&with.id).await.unwrap().unwrap().deleted_at.is_none());
    assert!(scratch.store.file(&early.id).await.unwrap().unwrap().deleted_at.is_some());
}

#[tokio::test]
async fn remove_share_user_works_on_trashed_targets() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let file = scratch.store.insert_file(&user.id, None, "shared", b"data").await.unwrap();
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, true).await.unwrap();
    // Trash does not stop an unshare: revoke while trashed, then restore
    // to find the grant gone.
    scratch.store.delete_file(&file.id).await.unwrap();
    scratch.store.remove_share_user(ShareKind::File, &file.id, &friend.id).await.unwrap();
    scratch.store.restore_file(&file.id).await.unwrap();
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &friend.id).await.unwrap());
    // Purged or missing targets still refuse: there is nothing to unshare.
    scratch.store.delete_file(&file.id).await.unwrap();
    scratch.store.purge_file(&file.id).await.unwrap();
    assert!(matches!(
        scratch.store.remove_share_user(ShareKind::File, &file.id, &friend.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        scratch.store.remove_share_user(ShareKind::File, "missing", &friend.id).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn add_share_user_needs_the_owner() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let stranger = scratch.store.provision_user("sub-caro", "caro@example.com", "Caro", None, 0, 1024).await.unwrap();
    let file = scratch.store.insert_file(&user.id, None, "mine", b"data").await.unwrap();
    // A stranger naming someone else's file is refused — and names no grant.
    assert!(matches!(
        scratch.store.add_share_user(&friend.id, ShareKind::File, &file.id, &stranger.id, true).await,
        Err(StoreError::CrossOwner)
    ));
    assert!(!scratch.store.can_see(ShareKind::File, &file.id, &stranger.id).await.unwrap());
    // A missing target is not-found, like the sibling link creator answers.
    assert!(matches!(
        scratch.store.add_share_user(&user.id, ShareKind::File, "missing", &stranger.id, true).await,
        Err(StoreError::NotFound)
    ));
    // The owner shares freely.
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &stranger.id, true).await.unwrap();
    assert!(scratch.store.can_see(ShareKind::File, &file.id, &stranger.id).await.unwrap());
}

#[tokio::test]
async fn shares_for_target_lists_grants_for_the_owner() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let friend = bob(&scratch.store).await;
    let stranger = scratch.store.provision_user("sub-caro", "caro@example.com", "Caro", None, 0, 1024).await.unwrap();
    let file = scratch.store.insert_file(&user.id, None, "mine", b"data").await.unwrap();
    // No grants yet.
    assert!(scratch.store.shares_for_target(&user.id, ShareKind::File, &file.id).await.unwrap().is_empty());
    // A stranger may not list another owner's grants; a missing target is
    // not-found, like the sibling share writes answer.
    assert!(matches!(
        scratch.store.shares_for_target(&stranger.id, ShareKind::File, &file.id).await,
        Err(StoreError::CrossOwner)
    ));
    assert!(matches!(
        scratch.store.shares_for_target(&user.id, ShareKind::File, "missing").await,
        Err(StoreError::NotFound)
    ));
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &friend.id, true).await.unwrap();
    scratch.store.add_share_user(&user.id, ShareKind::File, &file.id, &stranger.id, false).await.unwrap();
    let grants = scratch.store.shares_for_target(&user.id, ShareKind::File, &file.id).await.unwrap();
    assert_eq!(grants.len(), 2);
    let flag_of = |id: &str| grants.iter().find(|grant| grant.user_id == id).map(|grant| grant.can_download);
    assert_eq!(flag_of(&friend.id), Some(true));
    assert_eq!(flag_of(&stranger.id), Some(false));
    // A trashed target lists as not-found, like the sibling share writes.
    scratch.store.delete_file(&file.id).await.unwrap();
    assert!(matches!(
        scratch.store.shares_for_target(&user.id, ShareKind::File, &file.id).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_provisioned_user_wears_the_default_preferences() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    assert_eq!(user.ui, "instrument");
    assert_eq!(user.theme, "dark");
    assert_eq!(user.language, "en");
}

#[tokio::test]
async fn preferences_round_trip_and_refuse_what_is_not_offered() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch
        .store
        .set_preferences(&user.id, "dark", "tr", "instrument")
        .await
        .unwrap();
    let back = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(back.theme, "dark");
    assert_eq!(back.language, "tr");
    assert_eq!(back.ui, "instrument");
    // A value no preferences form offers is refused, and the row keeps what
    // the last good write left — the same Corrupt the store answers a bad
    // enum it reads.
    assert!(matches!(
        scratch.store.set_preferences(&user.id, "dim", "tr", "instrument").await,
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        scratch.store.set_preferences(&user.id, "dark", "xx", "instrument").await,
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        scratch.store.set_preferences(&user.id, "dark", "tr", "dense").await,
        Err(StoreError::Corrupt(_))
    ));
    let kept = scratch.store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(kept.theme, "dark");
    assert_eq!(kept.language, "tr");
    assert_eq!(kept.ui, "instrument");
    assert!(matches!(
        scratch.store.set_preferences("no-such-user", "light", "en", "ledger").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_second_migration_database_gains_the_preference_columns_on_open() {
    // A database built from 0001+0002 — the shape in ran with an interface
    // picker and no theme or language — carries no `theme` or `language`
    // column. Opening it with the current code reconciles it onto the
    // declared schema: its interface choice is carried across, and the two
    // new preferences arrive wearing their defaults.
    let dir = std::env::temp_dir().join(format!("in-test-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("in.db").to_string_lossy().into_owned();
    {
        let db = turso::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))
            .await
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0002_ui.sql"))
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO user (id, oidc_sub, email, display_name, admin, disabled, \
             quota_bytes, used_bytes, ui, created_at, last_seen_at) \
             VALUES ('u-old', 'sub-old', 'old@example.com', 'Old', 1, 0, 100, 0, \
             'instrument', '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z')",
            (),
        )
        .await
        .unwrap();
    }
    let storage = dir.join("storage");
    let store = TursoStore::open(&path, Some(&storage)).await.unwrap();
    let user = store.user_by_oidc_sub("sub-old").await.unwrap().unwrap();
    assert_eq!(user.ui, "instrument");
    assert_eq!(user.theme, "dark");
    assert_eq!(user.language, "en");
    // And the carried row takes new preferences like any other.
    store
        .set_preferences(&user.id, "dark", "tr", "ledger")
        .await
        .unwrap();
    let back = store.user(&user.id).await.unwrap().unwrap();
    assert_eq!(back.theme, "dark");
    assert_eq!(back.language, "tr");
    assert_eq!(back.ui, "ledger");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn renaming_or_moving_a_folder_onto_a_live_sibling_name_postfixes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let parent = scratch.store.create_folder(&user.id, None, "parent").await.unwrap();
    let other = scratch.store.create_folder(&user.id, None, "other").await.unwrap();

    // Renaming onto a live sibling's name lands on the first free postfix.
    scratch.store.rename_folder(&other.id, "parent").await.unwrap();
    let back: Folder = scratch.store.folder(&other.id).await.unwrap().unwrap();
    assert_eq!(back.name, "parent (2)");

    // Moving under a parent is refused for strangers and the trashed, but
    // never for a name: a live child wearing the name is postfixed.
    let kid = scratch
        .store
        .create_folder(&user.id, Some(&parent.id), "kid")
        .await
        .unwrap();
    scratch.store.create_folder(&user.id, None, "kid").await.unwrap();
    scratch.store.move_folder(&kid.id, None).await.unwrap();
    let back: Folder = scratch.store.folder(&kid.id).await.unwrap().unwrap();
    assert_eq!(back.parent_id, None);
    assert_eq!(back.name, "kid (2)");
    let listing = scratch.store.list_children(&user.id, None).await.unwrap();
    assert_eq!(listing.folders.len(), 4);
    assert!(listing.folders.iter().any(|folder| folder.name == "kid"));
    assert!(listing.folders.iter().any(|folder| folder.name == "kid (2)"));
}

#[tokio::test]
async fn file_upload_collisions_postfix_at_the_last_dot() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let first = scratch.store.insert_file(&user.id, None, "report.txt", b"one").await.unwrap();
    assert_eq!(first.name, "report.txt");
    // A live sibling wearing the name is no refusal: the postfix splits at
    // the last dot, and the (2) slot occupied steps on to (3).
    let second = scratch.store.insert_file(&user.id, None, "report.txt", b"two").await.unwrap();
    assert_eq!(second.name, "report (2).txt");
    let third = scratch.store.insert_file(&user.id, None, "report.txt", b"three").await.unwrap();
    assert_eq!(third.name, "report (3).txt");
}

#[tokio::test]
async fn extension_less_file_collisions_postfix_at_the_end() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "note", b"one").await.unwrap();
    let second = scratch.store.insert_file(&user.id, None, "note", b"two").await.unwrap();
    assert_eq!(second.name, "note (2)");
}

#[tokio::test]
async fn file_rename_onto_a_sibling_name_postfixes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "report.txt", b"one").await.unwrap();
    scratch.store.insert_file(&user.id, None, "report (2).txt", b"two").await.unwrap();
    let other = scratch.store.insert_file(&user.id, None, "other.txt", b"three").await.unwrap();
    // Both the plain name and (2) are taken: the rename steps on to (3).
    scratch.store.rename_file(&other.id, "report.txt").await.unwrap();
    let back = scratch.store.file(&other.id).await.unwrap().unwrap();
    assert_eq!(back.name, "report (3).txt");
    // Renaming onto a free name keeps it whole.
    scratch.store.rename_file(&other.id, "fresh.txt").await.unwrap();
    let back = scratch.store.file(&other.id).await.unwrap().unwrap();
    assert_eq!(back.name, "fresh.txt");
}

#[tokio::test]
async fn file_move_into_a_folder_with_a_name_twin_postfixes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let folder = scratch.store.create_folder(&user.id, None, "docs").await.unwrap();
    scratch.store.insert_file(&user.id, Some(&folder.id), "report.txt", b"inside").await.unwrap();
    let loose = scratch.store.insert_file(&user.id, None, "report.txt", b"loose").await.unwrap();
    // Same name in a different directory is fine; moving it next to its
    // twin postfixes instead of refusing.
    scratch.store.move_file(&loose.id, Some(&folder.id)).await.unwrap();
    let back = scratch.store.file(&loose.id).await.unwrap().unwrap();
    assert_eq!(back.folder_id.as_deref(), Some(folder.id.as_str()));
    assert_eq!(back.name, "report (2).txt");
}

#[tokio::test]
async fn file_restore_onto_a_squatter_postfixes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let first = scratch.store.insert_file(&user.id, None, "note.txt", b"one").await.unwrap();
    // Trashing frees the name: the next upload takes it plain.
    scratch.store.delete_file(&first.id).await.unwrap();
    let squatter = scratch.store.insert_file(&user.id, None, "note.txt", b"squatter").await.unwrap();
    assert_eq!(squatter.name, "note.txt");
    // Restoring onto the squatter postfixes instead of refusing.
    scratch.store.restore_file(&first.id).await.unwrap();
    let back = scratch.store.file(&first.id).await.unwrap().unwrap();
    assert_eq!(back.name, "note (2).txt");
}

#[tokio::test]
async fn finish_upload_onto_a_taken_name_postfixes() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    scratch.store.insert_file(&user.id, None, "clip.txt", b"taken").await.unwrap();
    let bytes = b"arriving";
    let session = scratch
        .store
        .create_upload_session(&user.id, None, "clip.txt", bytes.len() as u64)
        .await
        .unwrap();
    scratch.store.record_chunk(&session.id, 0, bytes).await.unwrap();
    // The session keeps the user's literal name; the finished file wears
    // the first free postfix.
    let file = scratch.store.finish_upload(&session.id).await.unwrap();
    assert_eq!(file.name, "clip (2).txt");
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), bytes);
}

#[tokio::test]
async fn download_count_starts_at_zero_and_counts_serves() {
    let scratch = Scratch::open().await;
    let user = alice(&scratch.store).await;
    let bytes = b"count me";
    let file = scratch.store.insert_file(&user.id, None, "counted", bytes).await.unwrap();
    assert_eq!(file.download_count, 0);

    // The row agrees: a fresh insert was never served.
    let back = scratch.store.file(&file.id).await.unwrap().unwrap();
    assert_eq!(back.download_count, 0);

    scratch.store.record_download(&file.id).await.unwrap();
    scratch.store.record_download(&file.id).await.unwrap();
    let back = scratch.store.file(&file.id).await.unwrap().unwrap();
    assert_eq!(back.download_count, 2);

    // A byte read is pure: it serves the bytes without counting, so range
    // probes and chunks never inflate the count — only record_download does.
    let served = scratch.store.file_bytes(&file.id).await.unwrap().unwrap();
    assert_eq!(served, bytes);
    let back = scratch.store.file(&file.id).await.unwrap().unwrap();
    assert_eq!(back.download_count, 2);
    assert!(scratch.store.file_bytes("no-such-file").await.unwrap().is_none());
    assert!(matches!(
        scratch.store.record_download("no-such-file").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn settings_round_trip_from_missing() {
    let scratch = Scratch::open().await;
    let store = &scratch.store;
    assert_eq!(store.get_setting("ui.density").await.unwrap(), None);
    store.set_setting("ui.density", "ledger").await.unwrap();
    assert_eq!(
        store.get_setting("ui.density").await.unwrap(),
        Some("ledger".to_string())
    );
    // Replacing the value replaces the row, and other keys are untouched.
    store.set_setting("ui.density", "instrument").await.unwrap();
    assert_eq!(
        store.get_setting("ui.density").await.unwrap(),
        Some("instrument".to_string())
    );
    assert_eq!(store.get_setting("ui.other").await.unwrap(), None);
}

// -- the r2 backend ---------------------------------------------------------
//
// The flows the local tree is tested with, over an in-process S3 server:
// `s3s` serves a filesystem-backed store on the loopback, `R2Blobs` is the
// client, and `TursoStore` holds the rows. Nothing reaches past 127.0.0.1
// and nothing is gated on the environment: the server binds port 0.

use std::sync::Arc;

use futures_util::StreamExt;
use in_core::store::blobs::Blobs;
use in_core::store::r2::R2Blobs;

/// Spins the S3 test server: a filesystem store behind `s3s`, bound to an
/// OS-picked loopback port. Returns the endpoint and the server root, for
/// cleanup. The bucket directory is made up front — the server creates
/// buckets on CreateBucket, which an object-store client never calls, so
/// the directory is the bucket.
async fn s3_server() -> (String, PathBuf) {
    let root = std::env::temp_dir().join(format!("in-r2-server-{}", Ulid::new()));
    std::fs::create_dir_all(root.join("in-test")).unwrap();
    // The object-store client signs every request; without an auth provider
    // the server would answer 501 to all of them. The credentials here are
    // the same pair the client is opened with below.
    let auth = s3s::auth::SimpleAuth::from_single("test-key", "test-secret");
    // `set_auth` is `&mut self` in this s3s generation: it mutates in
    // place instead of chaining.
    let mut builder =
        s3s::service::S3ServiceBuilder::new(s3s_fs::FileSystem::new(&root).unwrap());
    builder.set_auth(auth);
    let service = builder.build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = service.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let http = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                let _ = http.serve_connection(io, service).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), root)
}

/// A store whose bytes live behind [`R2Blobs`], plus the pieces a test
/// needs around it: the blobs handle (to plant orphans and to read the
/// bucket the way the sweep's eyes do) and the scratch paths, cleaned up on
/// drop.
struct R2Scratch {
    dir: PathBuf,
    root: PathBuf,
    blobs: Arc<R2Blobs>,
    store: TursoStore,
}

impl R2Scratch {
    async fn open() -> Self {
        let (endpoint, root) = s3_server().await;
        let blobs = Arc::new(
            R2Blobs::open(&endpoint, "in-test", "test-key", "test-secret", true).unwrap(),
        );
        let dir = std::env::temp_dir().join(format!("in-r2-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("in.db");
        let storage = dir.join("storage");
        let store =
            TursoStore::open_with_blobs(db.to_str().unwrap(), Some(&storage), blobs.clone())
                .await
                .unwrap();
        Self {
            dir,
            root,
            blobs,
            store,
        }
    }
}

impl Drop for R2Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Collects a [`FileSpan`](in_core::store::FileSpan)'s frames.
async fn drain(span: in_core::store::FileSpan) -> Vec<u8> {
    let mut span = span;
    let mut out = Vec::new();
    while let Some(frame) = span.stream.next().await {
        out.extend_from_slice(&frame.unwrap());
    }
    out
}

#[tokio::test]
async fn r2_insert_round_trips_byte_identical() {
    let scratch = R2Scratch::open().await;
    let alice = alice(&scratch.store).await;
    let bytes: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    let file = scratch
        .store
        .insert_file(alice.id.as_str(), None, "data.bin", &bytes)
        .await
        .unwrap();
    assert_eq!(scratch.store.file_bytes(&file.id).await.unwrap().unwrap(), bytes);
}

#[tokio::test]
async fn r2_file_stream_serves_the_mid_file_range() {
    let scratch = R2Scratch::open().await;
    let alice = alice(&scratch.store).await;
    let bytes: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    let file = scratch
        .store
        .insert_file(alice.id.as_str(), None, "data.bin", &bytes)
        .await
        .unwrap();
    let span = scratch
        .store
        .file_stream(&file.id, 1000, 500)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(span.len, 500);
    let got = drain(span).await;
    assert_eq!(got, bytes[1000..1500]);
}

#[tokio::test]
async fn r2_purge_takes_the_key_and_the_bucket_forgets_it() {
    let scratch = R2Scratch::open().await;
    let alice = alice(&scratch.store).await;
    let file = scratch
        .store
        .insert_file(alice.id.as_str(), None, "gone.bin", b"trash me")
        .await
        .unwrap();
    scratch.store.delete_file(&file.id).await.unwrap();
    scratch.store.purge_file(&file.id).await.unwrap();
    assert!(scratch.blobs.list("files").await.unwrap().is_empty());
}

#[tokio::test]
async fn r2_the_sweep_takes_orphans_and_leaves_known_keys() {
    let scratch = R2Scratch::open().await;
    let alice = alice(&scratch.store).await;
    // A client-side PUT with no row behind it — the exact shape the boot
    // sweep's orphan pass exists to clean. It lands BEFORE the row below,
    // which is what makes it older than the database's newest moment and so
    // reclaimable; an object newer than that is an upload the database has
    // not heard of and is kept.
    scratch
        .blobs
        .put("files/orphaned-by-crash", b"stray")
        .await
        .unwrap();
    let file = scratch
        .store
        .insert_file(alice.id.as_str(), None, "kept.bin", b"kept bytes")
        .await
        .unwrap();
    // `boot_sweeps` runs the orphan sweep over the bucket, exactly as the
    // server does in its background task.
    scratch.store.boot_sweeps().await.unwrap();
    let names = scratch.blobs.list("files").await.unwrap();
    assert!(!names.iter().any(|name| name.contains("orphaned-by-crash")));
    // The known key is untouched — the sweep deletes what no row names,
    // never what a row names.
    assert_eq!(
        scratch.store.file_bytes(&file.id).await.unwrap().unwrap(),
        b"kept bytes"
    );
}

#[tokio::test]
async fn r2_thumbnails_round_trip() {
    let scratch = R2Scratch::open().await;
    let alice = alice(&scratch.store).await;
    let file = scratch
        .store
        .insert_file(alice.id.as_str(), None, "red.png", &png_bytes())
        .await
        .unwrap();
    assert_eq!(file.thumb_state, ThumbState::Ready);
    let served = scratch.store.thumb_bytes(&file.id).await.unwrap().unwrap();
    assert!(!served.is_empty());
    // The same bytes sit at the thumbnail key in the bucket.
    let stored = scratch
        .blobs
        .get(&format!("thumbs/{}", file.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, served);
}
