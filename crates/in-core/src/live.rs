//! Committed-write announcements.
//!
//! A write that committed publishes one [`Change`] per surface it touches,
//! and the change carries a topic — never data. That is deliberate: a
//! channel that carried rows would need a per-message authorisation check,
//! and one forgotten filter would leak. A topic cannot leak anything; the
//! woken client re-fetches through the ordinary route, and the existing gate
//! answers there.
//!
//! [`Change::seq`] is a process-local counter, so a client that sees 41 and
//! then 43 knows it missed one and can resync instead of rendering a mix of
//! before and after.

use std::sync::atomic::{AtomicU64, Ordering};

/// The surface a committed write touched. Every variant carries the user the
/// surface belongs to: a change is only ever heard by its own user — or, for
/// [`Topic::Admin`], by an admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    /// The person's library: folders, files, uploads, thumbnails.
    Library(String),
    /// The person's trash: what was trashed, restored, purged or emptied.
    Trash(String),
    /// Shares: links created or revoked, per-person grants added or removed.
    Shares(String),
    /// Users, quotas, disablements — the admin's surface.
    Admin(String),
}

impl Topic {
    /// The name the wire uses for this topic: the SSE `event:` field, and
    /// the key the listener filter decides on.
    pub fn kind(&self) -> &'static str {
        match self {
            Topic::Library(_) => "library",
            Topic::Trash(_) => "trash",
            Topic::Shares(_) => "shares",
            Topic::Admin(_) => "admin",
        }
    }

    /// The user this topic is about. A listener may hear a topic when this
    /// is its own id — or, for [`Topic::Admin`], when it is an admin.
    pub fn id(&self) -> &str {
        match self {
            Topic::Library(id) | Topic::Trash(id) | Topic::Shares(id) | Topic::Admin(id) => id,
        }
    }
}

/// One committed write: which surface changed, and where this change sits
/// in the store's sequence. Everything a client needs to know to refresh —
/// and nothing about what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub topic: Topic,
    pub seq: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The next sequence number, starting at 1. Process-local on purpose: the
/// sequence exists to catch a dropped event inside one stream, not to order
/// two servers.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}
