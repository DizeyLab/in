// topcoat's `view!` nests as deeply as the pages do for the same drive, so
// the raised limit carries over from day one rather than waiting for the
// first overflow.
#![recursion_limit = "256"]

pub mod avatar;
pub mod drive;
pub mod dropdown;
pub mod files;
pub mod i18n;
pub mod layout;
pub mod live;
pub mod pages;
pub mod server;
pub mod settings;
pub mod share;
pub mod trash;
pub mod upload;
