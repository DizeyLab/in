//! Domain model and storage for In.
//!
//! The vocabulary — the account, folder, file, share and upload shapes —
//! compiles everywhere, so the UI and the server speak the same types. The
//! `server` feature adds the halves that must never reach the browser: the
//! store, the config, the live announcements and the thumbnailing.

pub mod store;

#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod live;
#[cfg(feature = "server")]
pub mod thumbs;

pub use store::{
    CHUNK_SIZE, CreatedLink, File, Folder, Listing, ShareKind, ShareLink, ShareUser, SharedItem,
    ThumbState, UPLOAD_TTL_HOURS, UploadSession, UploadState, User,
};

#[cfg(feature = "server")]
pub use config::{Config, ConfigError, OidcConfig};
#[cfg(feature = "server")]
pub use live::{Change, Topic, next_seq};
#[cfg(feature = "server")]
pub use store::{
    ReconcileOptions, Result, Store, StoreError, TursoStore, hash_share_token, reconcile,
};
