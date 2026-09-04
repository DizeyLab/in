//! Storage boundary.
//!
//! Everything the app does to persistent state goes through [`Store`]. The
//! only implementation today is Turso (in-process, SQLite-compatible); a
//! Postgres swap is a new impl of this trait and nothing else.
//!
//! The record shapes — [`User`], [`Folder`], [`File`] and the rest — are the
//! vocabulary and compile everywhere. Everything that touches a database, a
//! secret or a CSPRNG lives behind `server` so none of it is shipped to the
//! browser.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[cfg(feature = "server")]
pub mod reconcile;
#[cfg(feature = "server")]
pub mod schema;
#[cfg(feature = "server")]
pub mod secret;
#[cfg(feature = "server")]
pub mod sniff;
#[cfg(feature = "server")]
pub mod turso_store;

#[cfg(feature = "server")]
pub use turso_store::{TursoStore, hash_share_token};
#[cfg(feature = "server")]
pub use reconcile::{ReconcileOptions, reconcile};

/// What the store can fail with. A caller that needs to distinguish "no such
/// row" from "someone else's row" gets [`StoreError::NotFound`] for the
/// first and [`StoreError::CrossOwner`] for the second — and the route layer
/// answers 404 to both, because telling a stranger which ids exist is the
/// leak the distinction exists to prevent.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The name is already taken among the live siblings.
    #[error("that name is already taken here")]
    NameTaken,
    /// The database refused the write or could not be read.
    #[error("database: {0}")]
    Backend(String),
    /// No such row.
    #[error("not found")]
    NotFound,
    /// The row belongs to another owner. Answered as 404, never 403.
    #[error("not found")]
    CrossOwner,
    /// The owner's quota has no room for these bytes.
    #[error("quota exceeded")]
    QuotaExceeded,
    /// The upload session is past `expires_at`, or is no longer active.
    #[error("upload expired")]
    UploadExpired,
    /// A chunk that does not fit the session: wrong size, wrong index, or
    /// bytes past the declared total.
    #[error("bad chunk")]
    BadChunk,
    /// The restore is refused: an ancestor is still trashed, and restoring
    /// under it would resurrect the row into a trash it never left.
    #[error("an ancestor is still trashed")]
    AncestorTrashed,
    /// The folder move would put a folder inside its own descendant.
    #[error("that move would make a circle")]
    Cycle,
    /// A stored value is not valid — a timestamp that does not parse, a state
    /// the code does not know.
    #[error("stored value is not valid: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// An account. There is no password here on purpose: sign-in is the OIDC
/// provider's business, and this record is what handlers load and what pages
/// serialise, so a secret with a field here is a secret one careless response
/// away from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    /// The provider's stable subject. The row is created on first sight of a
    /// sub and refreshed on every sight after.
    pub oidc_sub: String,
    pub email: String,
    pub display_name: String,
    pub admin: bool,
    pub disabled: bool,
    /// The person's ceiling, in bytes. Set at provisioning, changed by an
    /// admin afterwards.
    pub quota_bytes: u64,
    /// Every live AND trashed byte the person holds. Trash counts, because a
    /// file that can be restored still costs. Recomputed from the rows after
    /// every mutation, never blindly incremented.
    pub used_bytes: u64,
    /// Display-only, the way `quota_bytes` is data: 'ledger' or 'instrument',
    /// read by `root_layout` into `data-ui`. Nothing stored depends on it.
    pub ui: String,
    pub created_at: OffsetDateTime,
    pub last_seen_at: Option<OffsetDateTime>,
}

/// A folder. `parent_id` NULL is the person's root. `deleted_at` is the
/// trash: `Some` while trashed, with the same timestamp its whole trashed
/// subtree wears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub owner_id: String,
    pub parent_id: Option<String>,
    /// What the person called it. A label, never a path.
    pub name: String,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

/// A file, as a screen lists it: a name, a type, a size and where it sits.
/// The bytes are deliberately not on this type — it is what handlers load and
/// pages serialise, and a file's contents have no business travelling with a
/// list of file names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub id: String,
    pub owner_id: String,
    pub folder_id: Option<String>,
    /// What the person called it. A label, never a path.
    pub name: String,
    /// What the server decided the bytes are, never what the upload claimed.
    pub mime: String,
    pub size_bytes: u64,
    pub thumb_state: ThumbState,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

/// Whether a file's thumbnail exists. `None` for bytes no thumbnail is
/// attempted for; `Pending` while one is being made; `Ready` or `Failed`
/// once the attempt settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThumbState {
    None,
    Pending,
    Ready,
    Failed,
}

impl ThumbState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThumbState::None => "none",
            ThumbState::Pending => "pending",
            ThumbState::Ready => "ready",
            ThumbState::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "none" => Ok(ThumbState::None),
            "pending" => Ok(ThumbState::Pending),
            "ready" => Ok(ThumbState::Ready),
            "failed" => Ok(ThumbState::Failed),
            _ => Err(StoreError::Corrupt(format!("thumb state {raw:?}"))),
        }
    }
}

/// Which table a share points at — one file, or one folder and everything
/// under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    File,
    Folder,
}

impl ShareKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareKind::File => "file",
            ShareKind::Folder => "folder",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "file" => Ok(ShareKind::File),
            "folder" => Ok(ShareKind::Folder),
            _ => Err(StoreError::Corrupt(format!("share kind {raw:?}"))),
        }
    }
}

/// A public share link. The row holds only the token's hash; the plaintext
/// is shown once at creation and never again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLink {
    pub id: String,
    pub token_hash: String,
    pub kind: ShareKind,
    pub target_id: String,
    pub created_by: String,
    pub can_download: bool,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
}

impl ShareLink {
    /// Whether the link opens today: unrevoked and unexpired.
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > now)
    }
}

/// A freshly created link, with the one thing the row does not keep: the
/// plaintext token, shown once and then forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedLink {
    pub link: ShareLink,
    pub token: String,
}

/// One file or folder shared with one person. The row is the whole of the
/// permission — removing it unshares completely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareUser {
    pub kind: ShareKind,
    pub target_id: String,
    pub user_id: String,
    pub can_download: bool,
    pub created_at: OffsetDateTime,
}

/// A target shared with the reader, with the name the listing prints. What a
/// "shared with me" screen reads; the owner's own library never appears here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedItem {
    pub kind: ShareKind,
    pub target_id: String,
    pub name: String,
    /// The file's mime, when the target is one.
    pub mime: Option<String>,
    pub owner_id: String,
    pub can_download: bool,
    pub created_at: OffsetDateTime,
}

/// A chunked upload on its way in. `state` is `active` while chunks are
/// still arriving, `done` once the finish assembled them, `aborted` once
/// anyone gave up on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSession {
    pub id: String,
    pub owner_id: String,
    pub folder_id: Option<String>,
    /// What the person called the coming file. A label, never a path.
    pub name: String,
    /// The total the finish must add up to, in bytes.
    pub size_bytes: u64,
    /// The server-fixed chunk size every non-final chunk must match.
    pub chunk_size: u64,
    /// The bytes staged so far.
    pub received_bytes: u64,
    pub state: UploadState,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// What an upload session is doing: arriving, arrived, or given up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadState {
    Active,
    Done,
    Aborted,
}

impl UploadState {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadState::Active => "active",
            UploadState::Done => "done",
            UploadState::Aborted => "aborted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "active" => Ok(UploadState::Active),
            "done" => Ok(UploadState::Done),
            "aborted" => Ok(UploadState::Aborted),
            _ => Err(StoreError::Corrupt(format!("upload state {raw:?}"))),
        }
    }
}

/// One directory's live contents — or one search's live hits. Trashed rows
/// never appear here; the trash has its own listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub folders: Vec<Folder>,
    pub files: Vec<File>,
}

/// The chunk size the server fixes for every upload session: 8 MiB. The
/// upload route caps bodies above it, and the UI sends single-POST below it.
pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024;

/// How long an upload session gets before it expires: a day is long enough
/// for a browser left open overnight and short enough that an abandoned
/// upload cannot pin disk for a season.
pub const UPLOAD_TTL_HOURS: u32 = 24;

#[cfg(feature = "server")]
#[async_trait::async_trait]
pub trait Store: 'static + Send + Sync {
    /// A receiver of committed-write announcements. One [`Change`](crate::live::Change)
    /// per write that committed, carrying the topic to re-fetch and no data:
    /// the channel cannot say more than the reader may hear, because it says
    /// nothing — the woken client re-fetches through the ordinary gated
    /// route. Sending to zero subscribers is normal and silent, and a slow
    /// subscriber's overflow is the client's cue to resync.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::live::Change>;

    // -- users -------------------------------------------------------------

    /// The account holding `sub`, if it has ever signed in.
    async fn user_by_oidc_sub(&self, sub: &str) -> Result<Option<User>>;

    /// Lookup by address, folded to lowercase before comparing — the provider
    /// may capitalise what the row stored plainly, and the two must still
    /// meet. What the share UI uses to turn a typed address into a grantee.
    async fn user_by_email(&self, email: &str) -> Result<Option<User>>;

    /// One account, by id.
    async fn user(&self, id: &str) -> Result<Option<User>>;

    /// Every account, oldest first — the admin's settings screen.
    async fn users(&self) -> Result<Vec<User>>;

    /// JIT upsert on OIDC login: unknown sub inserts (first user ever gets
    /// admin=1, quota = default_quota_bytes), known sub refreshes
    /// email/display_name and stamps last_seen_at. Returns the row.
    async fn provision_user(
        &self,
        sub: &str,
        email: &str,
        display_name: &str,
        default_quota_bytes: u64,
    ) -> Result<User>;

    /// Writes an account's ceiling. The bytes it already holds are untouched
    /// — a quota lowered under current usage refuses new uploads until the
    /// person frees space, rather than deleting anything.
    async fn set_user_quota(&self, user_id: &str, quota_bytes: u64) -> Result<()>;

    /// Disables an account. A disabled account signs in to nothing: the auth
    /// layer reads this and treats the person as signed out.
    async fn set_user_disabled(&self, user_id: &str, disabled: bool) -> Result<()>;

    /// Writes the person's UI density: `ledger` or `instrument`, read by
    /// `root_layout` into `data-ui`. Anything else is refused with
    /// [`StoreError::Corrupt`] — the same refusal a bad enum read from the
    /// row gets, because a bad enum written is a corrupt row.
    async fn set_user_ui(&self, id: &str, ui: &str) -> Result<()>;

    // -- folders -----------------------------------------------------------

    /// Creates a folder. A parent of another owner is [`StoreError::CrossOwner`],
    /// not a refusal — the asker named a thing that is not theirs.
    async fn create_folder(
        &self,
        owner_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Folder>;

    /// One folder, by id, trashed or not. The caller decides what trashed
    /// means for its screen.
    async fn folder(&self, id: &str) -> Result<Option<Folder>>;

    /// Renames a folder. A name a live sibling wears is [`StoreError::NameTaken`].
    async fn rename_folder(&self, id: &str, name: &str) -> Result<()>;

    /// Moves a folder. Into its own descendant is [`StoreError::Cycle`];
    /// across owners is [`StoreError::CrossOwner`].
    async fn move_folder(&self, id: &str, parent_id: Option<&str>) -> Result<()>;

    /// Trashes a folder: the folder, every descendant folder and every file
    /// under them wear the same `deleted_at`, in one transaction. The bytes
    /// stay on disk until a purge takes the rows.
    async fn delete_folder(&self, id: &str) -> Result<()>;

    /// One directory's live contents: the folders and files whose parent is
    /// `parent_id` (`None` is the root) and whose trash timestamp is empty.
    async fn list_children(
        &self,
        owner_id: &str,
        parent_id: Option<&str>,
    ) -> Result<Listing>;

    // -- files -------------------------------------------------------------

    /// Stores small bytes straight through: sanitises the name, checks the
    /// quota, sniffs the mime (never trusting the uploader), writes the
    /// file, attempts a thumbnail for images and video, and recomputes usage. The
    /// chunked upload's finish is the same pipeline over assembled chunks.
    async fn insert_file(
        &self,
        owner_id: &str,
        folder_id: Option<&str>,
        name: &str,
        bytes: &[u8],
    ) -> Result<File>;

    /// One file's row, still without its bytes.
    async fn file(&self, id: &str) -> Result<Option<File>>;

    /// Renames a file. A name a live sibling wears is [`StoreError::NameTaken`].
    async fn rename_file(&self, id: &str, name: &str) -> Result<()>;

    /// Moves a file. Across owners is [`StoreError::CrossOwner`].
    async fn move_file(&self, id: &str, folder_id: Option<&str>) -> Result<()>;

    /// Trashes a file. The bytes stay on disk — and counting toward quota —
    /// until a purge takes the row.
    async fn delete_file(&self, id: &str) -> Result<()>;

    /// The bytes themselves, for the one handler that serves them. `None`
    /// when there is no such row, or its file went missing.
    async fn file_bytes(&self, id: &str) -> Result<Option<Vec<u8>>>;

    /// The thumbnail's bytes, for the one handler that serves them. `None`
    /// when no thumbnail was made, or its file went missing.
    async fn thumb_bytes(&self, id: &str) -> Result<Option<Vec<u8>>>;

    // -- trash -------------------------------------------------------------

    /// Everything the person trashed and has not purged: folders and files
    /// wearing a `deleted_at`, newest trash first.
    async fn list_trash(&self, owner_id: &str) -> Result<Listing>;

    /// Restores a trashed file. Refused with [`StoreError::AncestorTrashed`]
    /// while any ancestor folder is still trashed, and with
    /// [`StoreError::NameTaken`] while a live sibling wears its name.
    async fn restore_file(&self, id: &str) -> Result<()>;

    /// Restores a trashed folder and everything under it that it took with
    /// it — descendants wearing the folder's own trash timestamp. Anything
    /// trashed individually before the folder keeps its earlier moment and
    /// stays trashed. Same refusals as [`Store::restore_file`].
    async fn restore_folder(&self, id: &str) -> Result<()>;

    /// Purges one trashed file for good: the row goes and the bytes and
    /// thumbnail follow. `false` when there was no such row. A live file is
    /// never purged — trash it first.
    async fn purge_file(&self, id: &str) -> Result<bool>;

    /// Purges one trashed folder for good: the folder row, every trashed
    /// descendant folder and every trashed file under them, deepest rows
    /// first. Links and grants naming the purged rows go with them; bytes
    /// and thumbnails follow the commit. Returns how many rows went. A
    /// folder that is not trashed is refused with [`StoreError::NotFound`] —
    /// trash it first.
    async fn purge_folder(&self, id: &str) -> Result<u64>;

    /// Purges every file and folder trashed before `before`, deepest folders
    /// first, and returns how many rows went. The boot sweep calls this with
    /// "purge_after_days ago".
    async fn purge_expired(&self, before: OffsetDateTime) -> Result<u64>;

    /// Purges everything the person trashed, files before folders, and
    /// returns how many rows went.
    async fn empty_trash(&self, owner_id: &str) -> Result<u64>;

    // -- share links -------------------------------------------------------

    /// Creates a public link onto one file or folder. Returns the row and the
    /// plaintext token — the only moment the token exists outside its hash.
    /// A target of another owner is [`StoreError::CrossOwner`].
    async fn create_share_link(
        &self,
        created_by: &str,
        kind: ShareKind,
        target_id: &str,
        can_download: bool,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<CreatedLink>;

    /// Revokes a link. The row stays, so a dead link reads as revoked rather
    /// than wrong. Revoking twice is not an error.
    async fn revoke_share_link(&self, id: &str) -> Result<()>;

    /// Every link the person created, newest first — the shares screen.
    async fn share_links(&self, owner_id: &str) -> Result<Vec<ShareLink>>;

    /// Resolves a token hash to its link, if the link is live: unrevoked and
    /// unexpired at `now`. A dead link and a wrong token both answer `None`,
    /// because telling a stranger which tokens exist is the leak the
    /// distinction would be.
    async fn resolve_share_link(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ShareLink>>;

    // -- per-person shares -------------------------------------------------

    /// Shares one file or folder with one person. The caller must own the
    /// live target — another owner's target is [`StoreError::CrossOwner`],
    /// a missing or trashed one [`StoreError::NotFound`], the same rule as
    /// [`Store::create_share_link`]. Sharing with the owner, or
    /// re-sharing what is already shared, is not an error — the second write
    /// only refreshes `can_download`.
    async fn add_share_user(
        &self,
        caller_id: &str,
        kind: ShareKind,
        target_id: &str,
        user_id: &str,
        can_download: bool,
    ) -> Result<()>;

    /// Unshares. Removing what was never shared is not an error.
    async fn remove_share_user(
        &self,
        kind: ShareKind,
        target_id: &str,
        user_id: &str,
    ) -> Result<()>;

    /// Everything shared with the person that is still live: untrashed
    /// targets, newest grant first. The person's own library never appears
    /// here.
    async fn shares_for_user(&self, user_id: &str) -> Result<Vec<SharedItem>>;

    /// Whether `user_id` may see the target: its owner may, and anyone
    /// holding a live grant onto the target — or onto any live folder above
    /// it — may. A grant on a folder covers everything under it. Nobody else
    /// may — and the answer never says which of "no such target" and "not
    /// shared" it was.
    async fn can_see(&self, kind: ShareKind, target_id: &str, user_id: &str)
    -> Result<bool>;

    /// Whether `user_id` may download the target: its owner always may, and
    /// anyone may who sees it through a grant whose `can_download` is set —
    /// on the target itself or on any live folder above it. The most
    /// permissive grant on the chain wins: a download grant above the target
    /// opens the bytes even under a view-only grant on the target itself.
    /// A view-only grant alone opens the page but not the bytes.
    async fn can_download(
        &self,
        kind: ShareKind,
        target_id: &str,
        user_id: &str,
    ) -> Result<bool>;

    // -- chunked uploads ---------------------------------------------------

    /// Opens an upload session: checks the quota against the declared total,
    /// and answers [`StoreError::QuotaExceeded`] while there is still nothing
    /// on disk to clean up. A folder of another owner is
    /// [`StoreError::CrossOwner`].
    async fn create_upload_session(
        &self,
        owner_id: &str,
        folder_id: Option<&str>,
        name: &str,
        size_bytes: u64,
    ) -> Result<UploadSession>;

    /// One session, whatever its state.
    async fn upload_session(&self, id: &str) -> Result<Option<UploadSession>>;

    /// Stages one chunk: `bytes` land at `uploads/<id>/<index>`, and the
    /// session's received count follows the files on disk, not the calls —
    /// re-sending an index replaces its bytes rather than counting them
    /// twice. Anything past the declared total, or on a session that is not
    /// active and unexpired, is refused.
    async fn record_chunk(
        &self,
        id: &str,
        index: u64,
        bytes: &[u8],
    ) -> Result<UploadSession>;

    /// Assembles a session's chunks in order, rechecks the quota, sniffs the
    /// mime (never trusting anything the uploader said), writes the file,
    /// attempts a thumbnail for images and video, inserts the row and marks the session
    /// done — and returns the file. A session whose chunks do not add up, or
    /// whose owner filled their quota while it was arriving, is refused with
    /// the chunks still staged for another attempt.
    async fn finish_upload(&self, id: &str) -> Result<File>;

    /// Aborts a session: the row says `aborted` and the staged chunks are
    /// deleted. Aborting twice is not an error.
    async fn abort_upload(&self, id: &str) -> Result<()>;

    /// Aborts every active session past `expires_at` and deletes its chunks.
    /// The boot sweep calls this with now; a janitor may call it any time.
    async fn prune_expired_uploads(&self, now: OffsetDateTime) -> Result<u64>;

    // -- search ------------------------------------------------------------

    /// Owner-scoped substring search over live file and folder names, files
    /// and folders in their own lists. At most `limit` of each.
    async fn search(
        &self,
        owner_id: &str,
        query: &str,
        limit: u32,
    ) -> Result<Listing>;
}
