//! Everything in reads from `config/in.toml`, in one place.
//!
//! Nothing here has a silent default once the file exists. A key that is
//! missing, empty or unusable stops the boot and says which key and which
//! file, because the alternative is worse than not starting: a wrong
//! `database` does not mean "no data", it means a second in quietly
//! writing a different file while everyone believes they are looking at the
//! same drive — and Turso is single-writer, so the two are not even
//! reconcilable afterwards.
//!
//! Development still needs to be one command, so the *absence* of
//! `config/in.toml` is the opt-in that takes the development defaults: the
//! app writes the file itself, with those defaults in it and comments saying
//! what each key does, and then stops — the `[oidc]` keys have no default
//! worth guessing, so the first boot writes the file and says which keys to
//! fill in rather than starting half-authenticated. That is opt-in on purpose
//! too — it only ever happens once, because the second boot finds the file
//! it wrote the first time and reads it like any other. A real deployment is
//! handed the same file and edits it; it never writes itself over a
//! deployment's choices, because it only writes when the file is not there
//! at all.
//!
//! Whatever is finally resolved is printed once at startup — the database
//! path absolute, the address bound, the issuer sign-ins go to — so "which
//! file are we on" is answered by the log rather than by someone's memory.
//!
//! The file is kept complete: a top-level key it does not mention is added,
//! with its comment and the default already in effect, so reading the file
//! is the way to learn what can be changed. `redirect_uri` is the one
//! exception: it defaults from `listen` on every boot, and freezing the
//! derived value into the file would desync it the day `listen` moves. A key
//! nobody here knows is named in the startup report rather than obeyed or
//! refused — a typo should be visible without being a boot failure, and a key
//! this file has stopped honouring must never half-configure anything.

use serde::Deserialize;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// The path, relative to the working directory, of the file `Config::load`
/// reads and, failing that, writes.
const FILE_NAME: &str = "config/in.toml";

/// What a freshly written `config/in.toml` says, comments included.
/// Development defaults, except for `[oidc]`: no issuer, id or secret has a
/// default worth guessing, so the file is written with blanks and the boot
/// stops until they are filled in.
const DEVELOPMENT_DEFAULTS: &str = r#"# Where the one database file lives. One process holds it.
database = "in.db"
# The address the server listens on. Environment variables are ignored —
# this is the only thing that decides where in binds.
listen = "127.0.0.1:7655"
# How long a live-update connection is held before the browser is asked to
# reconnect, in seconds. The reconnect is what re-checks the session, so a
# revoked sign-in stops receiving within this long.
live_seconds = 300
# How many days a trashed file or folder waits before the boot sweep purges
# it for good. Trash counts toward quota until then.
purge_after_days = 30
# The quota a newly provisioned account starts on, in bytes. 10 GiB.
default_quota_bytes = 10737418240
# The base of the share links in hands out. Set it when in answers on a
# different address than it binds — a domain, or a proxy in front. Left
# empty, links follow the listen address.
base_url = ""
# Where drive files, thumbnails and staged upload chunks live as files,
# created on boot. Not written here: it defaults to beside the database file,
# and the boot completes this file with the value it resolved.
[oidc]
# The provider that signs people in. Required: no default is guessed.
issuer = ""
# The client id in presents to the provider. Required.
client_id = ""
# The client secret in presents to the provider. Required. The file holds a
# live credential once this is filled in, so it is created mode 0600.
client_secret = ""
# Where the provider sends the browser back after sign-in. Not written here:
# it defaults from `listen` on every boot, so moving the address never leaves
# a stale callback behind. Set it only when the public address differs from
# the bind address (a proxy in front, for instance).
# redirect_uri = "http://127.0.0.1:7655/auth/callback"
"#;

/// The default `listen` when the file is silent about it. A file missing this
/// key is completed with it on the next boot, so the silence lasts one run.
///
/// 7655 rather than a round number: 3000, 4000, 5000, 8000 and 8080 are what
/// every other thing on a developer's machine already took, and a first run
/// that dies on "address already in use" is a first run that teaches nothing.
const DEFAULT_LISTEN: &str = "127.0.0.1:7655";

/// How long one live-update connection lasts before the server ends it and the
/// browser opens another. Five minutes is a compromise: a stream is
/// authenticated once, when it opens, so a session revoked mid-stream keeps
/// hearing until the next reconnect — and a reconnect costs one request, so
/// doing it every few seconds to shorten that window would be worse than the
/// window. Long enough to be cheap, short enough that a revoked session goes
/// quiet while the person who revoked it is still watching.
const DEFAULT_LIVE_SECONDS: u64 = 300;

/// How many days trash waits before the boot sweep purges it for good.
const DEFAULT_PURGE_AFTER_DAYS: u32 = 30;

/// The quota a newly provisioned account starts on: 10 GiB.
const DEFAULT_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Every completable top-level key, with the comment and default a file
/// missing it is completed with. `database` is not here: it has no default
/// worth guessing, so its absence stops the boot instead. `storage` is
/// neither: its default is derived from `database`, so its completed line is
/// built per-file rather than printed from a constant. `redirect_uri` is not
/// here either: it derives from `listen` on every boot (see the module docs).
const OPTIONAL_KEYS: &[(&str, &str)] = &[
    (
        "listen",
        concat!(
            "# The address the server listens on. Environment variables are ignored —\n",
            "# this is the only thing that decides where in binds.\n",
            "listen = \"127.0.0.1:7655\"\n"
        ),
    ),
    (
        "live_seconds",
        concat!(
            "# How long a live-update connection is held before the browser is asked to\n",
            "# reconnect, in seconds. The reconnect is what re-checks the session, so a\n",
            "# revoked sign-in stops receiving within this long.\n",
            "live_seconds = 300\n"
        ),
    ),
    (
        "purge_after_days",
        concat!(
            "# How many days a trashed file or folder waits before the boot sweep purges\n",
            "# it for good. Trash counts toward quota until then.\n",
            "purge_after_days = 30\n"
        ),
    ),
    (
        "default_quota_bytes",
        concat!(
            "# The quota a newly provisioned account starts on, in bytes. 10 GiB.\n",
            "default_quota_bytes = 10737418240\n"
        ),
    ),
    (
        "base_url",
        concat!(
            "# The base of the share links in hands out. Set it when in answers on a\n",
            "# different address than it binds — a domain, or a proxy in front. Left\n",
            "# empty, links follow the listen address.\n",
            "base_url = \"\"\n"
        ),
    ),
];

/// The `[oidc]` table of `config/in.toml`, before the values are checked.
/// Anything else the section says lands in `other`, which is read for its key
/// names only.
#[derive(Deserialize)]
struct OidcToml {
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
    #[serde(flatten, default)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The two shapes the `storage` key legally takes. TOML cannot carry a
/// `storage = "path"` value and a `[storage.r2]` table in one file — a key
/// cannot be both a value and a table — so the untagged enum reads whichever
/// is there, and a file claiming both fails to parse, which is the honest
/// answer.
#[derive(Deserialize)]
#[serde(untagged)]
enum StorageToml {
    /// `storage = "path"`: the bytes live in the local tree.
    Path(String),
    /// A `[storage]` table; today only `r2` lives under it.
    Table(StorageTableToml),
}

/// The `[storage]` table shape. An empty one is legal and means local.
#[derive(Deserialize)]
struct StorageTableToml {
    r2: Option<R2Toml>,
    #[serde(flatten, default)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The `[storage.r2]` table, before the values are checked. A table present
/// but missing a key is [`ConfigError::Missing`] — absence of the whole
/// table is the local backend, absence inside the table is a mistake.
#[derive(Deserialize)]
struct R2Toml {
    account_id: Option<String>,
    bucket: Option<String>,
    key_file: Option<String>,
    #[serde(flatten, default)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The shape of `config/in.toml`, before the values are checked. Anything
/// else the file says lands in `other`, which is read for its key names only
/// — enough for the report to say a key was seen and not obeyed.
#[derive(Deserialize)]
struct Toml {
    database: Option<String>,
    storage: Option<StorageToml>,
    listen: Option<String>,
    live_seconds: Option<u64>,
    purge_after_days: Option<u32>,
    default_quota_bytes: Option<u64>,
    base_url: Option<String>,
    oidc: Option<OidcToml>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The OIDC provider in trusts, and how in presents itself to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    /// The provider's issuer URL. Sign-ins from anywhere else are refused.
    pub issuer: String,
    /// The client id in presents to the provider.
    pub client_id: String,
    /// The client secret in presents to the provider. Never printed.
    pub client_secret: String,
    /// Where the provider sends the browser after sign-in. Defaults from
    /// `listen` when the file is silent about it.
    pub redirect_uri: String,
}

/// Cloudflare R2 as the blob backend, from the `[storage.r2]` table. The
/// database keeps every row; only the file bytes move into the bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct R2Config {
    /// The R2 account id: the endpoint is
    /// `https://{account_id}.r2.cloudflarestorage.com`.
    pub account_id: String,
    /// The bucket the blobs land in, under `files/` and `thumbs/` — the
    /// same layout the local tree uses, so one `rclone` maps between them.
    pub bucket: String,
    /// The key file, resolved against the config directory like every other
    /// path here. Two lines: the access key id, then the secret. Read by
    /// the web binary at boot, never logged.
    pub key_file: PathBuf,
}

/// What the process needs to know before it opens a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The database file, absolute, as a string — the form
    /// [`crate::store::TursoStore::open`] takes.
    pub database: String,
    /// Where drive files, thumbnails and staged upload chunks live as files,
    /// absolute. Bytes never sit in the database: this directory and the
    /// database file are backed up together, or not at all.
    pub storage: PathBuf,
    /// The R2 backend when `[storage.r2]` is set; `None` keeps the bytes in
    /// the local tree. `storage` still names a local directory either way:
    /// upload chunks stage there before `adopt` streams them up.
    pub r2: Option<R2Config>,
    /// The address the server binds. The only source for this — `HOST` and
    /// `PORT` environment variables are never read.
    pub listen: SocketAddr,
    /// How long one live-update connection is held open before the browser is
    /// asked to reconnect. The reconnect re-authenticates, which is how a
    /// session revoked mid-stream stops being fed.
    pub live_seconds: u64,
    /// How many days trash waits before the boot sweep purges it for good.
    pub purge_after_days: u32,
    /// The quota a newly provisioned account starts on, in bytes.
    pub default_quota_bytes: u64,
    /// The origin the public links carry — share links and the copy-once
    /// banners that mint them — when the file sets one. `None` follows the
    /// address bound.
    pub base_url: Option<String>,
    /// The OIDC provider in trusts.
    pub oidc: OidcConfig,
    /// Keys the file sets that nothing here reads, in the order the file
    /// gives them. Named at startup so a typo is visible.
    pub ignored: Vec<String>,
    /// Whether `config/in.toml` did not exist and was just written with the
    /// development defaults this boot.
    pub defaulted: bool,
}

/// Why the process is not starting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The file exists but is not valid TOML.
    Unparseable { why: String },
    /// A key is missing or set to an empty value.
    Missing(&'static str),
    /// A key is set to something the app cannot use.
    Invalid { key: &'static str, why: String },
    /// The file could not be read for a reason other than "it is not there"
    /// (permissions, for instance), or the default could not be written.
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unparseable { why } => {
                write!(f, "not starting: {FILE_NAME} is not valid TOML — {why}")
            }
            ConfigError::Missing(key) => {
                write!(
                    f,
                    "not starting: {FILE_NAME} has no {key}. Add it, or delete {FILE_NAME} to take the development defaults."
                )
            }
            ConfigError::Invalid { key, why } => {
                write!(
                    f,
                    "not starting: {FILE_NAME}'s {key} is set to something unusable — {why}"
                )
            }
            ConfigError::Io(why) => write!(f, "not starting: {why}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Reads `config/in.toml` from the current directory, writing it with
    /// the development defaults first if it is not there.
    pub fn load() -> Result<Config, ConfigError> {
        Config::load_from(Path::new("."))
    }

    /// The same reading, against any directory — which is how it is tested
    /// without a test being able to disturb another test's working
    /// directory, or another test's `config/in.toml`.
    pub fn load_from(dir: &Path) -> Result<Config, ConfigError> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config = Config::parse(&text, dir, false)?;
                complete(&path, &text, dir);
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        ConfigError::Io(format!("could not create {}: {err}", parent.display()))
                    })?;
                }
                std::fs::write(&path, DEVELOPMENT_DEFAULTS).map_err(|err| {
                    ConfigError::Io(format!("could not write {}: {err}", path.display()))
                })?;
                // The file will hold a live client secret once the admin
                // fills it in; close it now, before it does.
                let _ = crate::store::secret::restrict(&path);
                println!(
                    "in    wrote {FILE_NAME} with development defaults — fill in [oidc] for a real deployment"
                );
                Config::parse(DEVELOPMENT_DEFAULTS, dir, true)
            }
            Err(err) => Err(ConfigError::Io(format!(
                "could not read {}: {err}",
                path.display()
            ))),
        }
    }

    /// The reading itself, against a string — which is how it is tested
    /// without a test being able to disturb another test's filesystem.
    pub fn parse(text: &str, dir: &Path, defaulted: bool) -> Result<Config, ConfigError> {
        let toml: Toml = toml::from_str(text).map_err(|err| ConfigError::Unparseable {
            why: err.to_string(),
        })?;

        let value = |raw: Option<String>| raw.filter(|value| !value.trim().is_empty());

        let database = value(toml.database).ok_or(ConfigError::Missing("database"))?;
        let database_path = absolute(dir, Path::new(&database));

        // `storage` has two shapes TOML keeps mutually exclusive: a plain
        // value (the local bytes-tree directory) and a `[storage.r2]` table
        // (the bytes in a bucket). A table without `r2` inside, and no key
        // at all, are both the local default — the tree still names where
        // chunks stage. Absence of the table is what selects the local
        // backend, so the completion below must never add one.
        let mut r2: Option<R2Config> = None;
        let mut storage_ignored: Vec<String> = Vec::new();
        let storage = match toml.storage {
            Some(StorageToml::Path(raw)) => match value(Some(raw)) {
                Some(path) => absolute(dir, Path::new(&path)),
                None => default_storage(&database_path),
            },
            Some(StorageToml::Table(table)) => {
                storage_ignored
                    .extend(table.other.into_keys().map(|key| format!("storage.{key}")));
                if let Some(table) = table.r2 {
                    let account_id = value(table.account_id)
                        .ok_or(ConfigError::Missing("storage.r2.account_id"))?;
                    let bucket =
                        value(table.bucket).ok_or(ConfigError::Missing("storage.r2.bucket"))?;
                    let key_file = value(table.key_file)
                        .ok_or(ConfigError::Missing("storage.r2.key_file"))?;
                    storage_ignored.extend(
                        table.other.into_keys().map(|key| format!("storage.r2.{key}")),
                    );
                    // The key file is a path like `storage` is: written
                    // relative to the file that named it.
                    r2 = Some(R2Config {
                        account_id,
                        bucket,
                        key_file: absolute(dir, Path::new(&key_file)),
                    });
                }
                default_storage(&database_path)
            }
            None => default_storage(&database_path),
        };

        let listen = value(toml.listen).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen: SocketAddr = listen.parse().map_err(|err| ConfigError::Invalid {
            key: "listen",
            why: format!("{listen:?} is not a host:port address — {err}"),
        })?;

        // Zero would mean a stream that ends the moment it opens, which is a
        // reconnect loop rather than a live feed; the key is refused rather
        // than quietly corrected, because a deployment that meant to say
        // "never expire" should find out here and not at three in the morning.
        let live_seconds = toml.live_seconds.unwrap_or(DEFAULT_LIVE_SECONDS);
        if live_seconds == 0 {
            return Err(ConfigError::Invalid {
                key: "live_seconds",
                why: "0 would close every live connection as soon as it opened".to_string(),
            });
        }

        let purge_after_days = toml.purge_after_days.unwrap_or(DEFAULT_PURGE_AFTER_DAYS);
        let default_quota_bytes = toml.default_quota_bytes.unwrap_or(DEFAULT_QUOTA_BYTES);

        // The origin public links carry. It has to be joinable to a link —
        // scheme and host, no trailing slash — because the link is built as
        // `{base_url}/s/{token}`; a value that would make that join a lie
        // (`iz.sh`, `https://iz.sh/`) is refused rather than quietly patched,
        // like every key here.
        let base_url = value(toml.base_url);
        if let Some(base) = &base_url {
            Config::validate_base_url(base).map_err(|why| ConfigError::Invalid {
                key: "base_url",
                why,
            })?;
        }

        let oidc_toml = toml.oidc.ok_or(ConfigError::Missing("[oidc]"))?;
        let issuer = value(oidc_toml.issuer).ok_or(ConfigError::Missing("oidc.issuer"))?;
        let client_id = value(oidc_toml.client_id).ok_or(ConfigError::Missing("oidc.client_id"))?;
        let client_secret =
            value(oidc_toml.client_secret).ok_or(ConfigError::Missing("oidc.client_secret"))?;
        // Derived from the address bound, on every boot: a deployment behind
        // a proxy sets this key, and everyone else follows `listen` for free.
        let redirect_uri = value(oidc_toml.redirect_uri)
            .unwrap_or_else(|| format!("{}/auth/callback", listen_url_of(&listen)));

        let mut ignored: Vec<String> = toml.other.into_keys().collect();
        ignored.extend(storage_ignored);
        ignored.extend(oidc_toml.other.into_keys().map(|key| format!("oidc.{key}")));

        Ok(Config {
            database: database_path.display().to_string(),
            storage,
            r2,
            listen,
            live_seconds,
            purge_after_days,
            default_quota_bytes,
            base_url,
            oidc: OidcConfig {
                issuer,
                client_id,
                client_secret,
                redirect_uri,
            },
            ignored,
            defaulted,
        })
    }

    /// The address the server binds, as a URL.
    ///
    /// A bind that names no interface — `0.0.0.0`, `::` — answers everywhere
    /// and is reachable at none of it by name, so the loopback stands in: a
    /// link somebody on the box can click beats a link nobody can. Whoever
    /// puts in behind a proxy sets the real address where it is needed —
    /// `base_url` for the public links, `redirect_uri` in `[oidc]` for
    /// sign-ins — which is the only thing this defers to.
    pub fn listen_url(&self) -> String {
        listen_url_of(&self.listen)
    }

    /// The origin the public links are told: `base_url` when the file sets
    /// one, otherwise the address bound. Only public URL construction — the
    /// share links and their copy-once banners — asks this; sign-ins keep
    /// following `redirect_uri` and the bind never moves.
    pub fn public_origin(&self) -> String {
        match self.base_url.as_deref() {
            Some(base) => base.to_string(),
            None => self.listen_url(),
        }
    }

    /// The lines to print once at startup. Nothing secret is among them.
    pub fn report(&self) -> Vec<String> {
        let mut lines = vec![
            format!("database  {}", self.database),
            format!("storage   {}", self.storage.display()),
            format!(
                "blobs     {}",
                match &self.r2 {
                    Some(r2) => format!("r2 {}", r2.bucket),
                    None => "local".to_string(),
                }
            ),
            format!("listen    {}", self.listen),
            format!("oidc      {}", self.oidc.issuer),
        ];
        if !self.ignored.is_empty() {
            lines.push(format!(
                "ignored   {FILE_NAME} sets {}, which nothing reads",
                self.ignored.join(", ")
            ));
        }
        if self.defaulted {
            lines.push(format!(
                "dev       {FILE_NAME} did not exist, development defaults written and taken"
            ));
        }
        lines
    }
}

/// The address bound, as a URL — the shape `redirect_uri` defaults from.
fn listen_url_of(listen: &SocketAddr) -> String {
    let ip = listen.ip();
    let host = if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else if listen.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    match listen.port() {
        80 => format!("http://{host}"),
        port => format!("http://{host}:{port}"),
    }
}

impl Config {
    /// Whether a base URL is an origin a public link can be built on: an
    /// `http` or `https` scheme and no trailing slash. Shared by the file's
    /// own parse and the admin's server-address form, so a value the boot
    /// refuses is refused by the form for exactly the same reason.
    pub fn validate_base_url(value: &str) -> Result<(), String> {
        if !value.starts_with("http://") && !value.starts_with("https://") {
            return Err(format!(
                "{value:?} names no scheme — an origin like https://files.example.com is what links are built from"
            ));
        }
        if value.ends_with('/') {
            return Err(format!(
                "{value:?} ends in a slash — set the bare origin; in joins the /s/ path itself"
            ));
        }
        Ok(())
    }
}

/// Adds the top-level keys the file does not mention, each with its own
/// comment and the default already in effect. The file is how a reader learns
/// what can be changed, so a key that is silently defaulted is a key nobody
/// discovers.
///
/// Only ever adds, never rewrites: whatever else the file says — comments,
/// ordering, a value somebody chose — is theirs. The additions go before the
/// first `[table]` header when there is one, because a key appended past one
/// would belong to that table rather than to the file. A file that cannot be
/// written is not a reason not to start; the defaults it is missing are the
/// ones already in force, so the run is correct either way and the note says
/// what could not be done.
fn complete(path: &Path, text: &str, base: &Path) {
    let mut missing: Vec<(&str, String)> = OPTIONAL_KEYS
        .iter()
        .filter(|(key, _)| !mentions(text, key))
        .map(|(key, block)| (*key, (*block).to_string()))
        .collect();
    // `storage` has no fixed default to print: a file silent about it derives
    // the directory from where `database` points, so the completed line says
    // the value this boot actually resolved rather than a placeholder.
    if !mentions(text, "storage") && !mentions_storage_table(text) {
        // The same base `parse` resolves against, or the completed line would
        // point somewhere this boot never looks (config/ vs the directory
        // the app runs from).
        let database: Option<String> = toml::from_str::<Toml>(text)
            .ok()
            .and_then(|t| t.database)
            .filter(|value| !value.trim().is_empty());
        let storage = database
            .as_deref()
            .map(|db| default_storage(&absolute(base, Path::new(db))))
            .unwrap_or_else(|| PathBuf::from("storage"));
        missing.push((
            "storage",
            format!(
                "# Where drive files, thumbnails and staged upload chunks live as files, created on boot.\n\
                 storage = {:?}\n",
                storage.display().to_string()
            ),
        ));
    }
    if missing.is_empty() {
        return;
    }
    let block = missing
        .iter()
        .map(|(_, block)| block.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // Top-level keys must land before any `[table]` header, or TOML would
    // read them as that table's. A file with no tables takes them at the end.
    let completed = match text.lines().position(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('[') && !trimmed.starts_with("[ ")
    }) {
        Some(idx) => {
            let mut lines: Vec<&str> = text.lines().collect();
            // A `[` that is not a header — `[[bin]]` aside, a bracketed value
            // cannot start a line in valid TOML outside a header — but only a
            // header matters here, and a false positive merely moves the
            // insertion up, never into another table.
            lines.insert(idx, block.as_str());
            let mut out = lines.join("\n");
            if text.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        None => {
            let mut completed = text.to_string();
            if !completed.is_empty() && !completed.ends_with('\n') {
                completed.push('\n');
            }
            completed.push('\n');
            completed.push_str(&block);
            completed
        }
    };
    match std::fs::write(path, completed) {
        Ok(()) => println!(
            "in    added {} to {FILE_NAME}, at the default already in use",
            missing
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(err) => println!("in    could not complete {FILE_NAME}: {err}"),
    }
}

/// Whether the file sets this top-level key — a line whose first word it is.
/// A key named inside a comment, a value, or an `[oidc]`-style table does not
/// count. (An indented `redirect_uri` inside `[oidc]` does count for its own
/// name: `trim_start` puts it at the line's start.)
fn mentions(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Whether the file carries a `[storage]` table at all — `[storage.r2]` or
/// any sibling under it. [`mentions`] cannot see one: a table header is not
/// a `key = value` line. The check matters because completing a file that
/// already has the table with a top-level `storage = "..."` would make the
/// next boot unparseable — one key cannot be both a value and a table.
fn mentions_storage_table(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start().strip_prefix('[').is_some_and(|rest| {
            rest.starts_with("storage]") || rest.starts_with("storage.")
        })
    })
}

/// Where binary files live when the file is silent about it: beside the
/// database file. The two are one backup unit — a backup that takes the
/// database but not the files beside it restores a drive whose contents are
/// gone — so the default keeps them siblings.
fn default_storage(database: &Path) -> PathBuf {
    database
        .parent()
        .map(|parent| parent.join("storage"))
        .unwrap_or_else(|| PathBuf::from("storage"))
}

/// An absolute path for a file that may not exist yet: the directory is
/// resolved against `base` (the directory `config/in.toml` was read
/// from), the file name is kept as written.
fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = base
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let joined = base.join(path);
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => joined.clone(),
        },
        _ => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory, cleaned up when the test ends, so `load_from` can
    /// be driven without touching the working directory.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("in-config-test-{}", Ulid::generate()));
            std::fs::create_dir_all(dir.join("config")).unwrap();
            Self { dir }
        }

        fn write(&self, text: &str) {
            std::fs::write(self.dir.join(FILE_NAME), text).unwrap();
        }

        fn load(&self) -> Result<Config, ConfigError> {
            Config::load_from(&self.dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    use ulid::Ulid;

    const FULL: &str = r#"database = "in.db"
listen = "127.0.0.1:7655"
live_seconds = 300
purge_after_days = 30
default_quota_bytes = 10737418240
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
"#;

    #[test]
    fn a_missing_file_is_written_with_defaults_and_then_refuses_oidc() {
        let scratch = Scratch::new();
        // The fresh file has blank [oidc] keys, so the first boot writes and
        // then stops on the first of them.
        let err = scratch.load().unwrap_err();
        assert_eq!(err, ConfigError::Missing("oidc.issuer"));
        let written = std::fs::read_to_string(scratch.dir.join(FILE_NAME)).unwrap();
        assert!(written.contains("database = \"in.db\""));
        assert!(written.contains("client_secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(scratch.dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the config file holds a live secret one day");
        }
    }

    #[test]
    fn a_full_file_loads() {
        let scratch = Scratch::new();
        scratch.write(FULL);
        let config = scratch.load().unwrap();
        assert!(config.database.ends_with("in.db"));
        assert_eq!(config.listen.port(), 7655);
        assert_eq!(config.live_seconds, 300);
        assert_eq!(config.purge_after_days, 30);
        assert_eq!(config.default_quota_bytes, 10 * 1024 * 1024 * 1024);
        assert_eq!(config.oidc.issuer, "https://id.example.com");
        assert_eq!(
            config.oidc.redirect_uri,
            "http://127.0.0.1:7655/auth/callback"
        );
        assert!(!config.defaulted);
        assert!(config.ignored.is_empty());
    }

    #[test]
    fn redirect_uri_defaults_from_listen() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("127.0.0.1:7655", "0.0.0.0:9000"));
        let config = scratch.load().unwrap();
        // An unspecified bind is not a name anyone can reach; the loopback
        // stands in.
        assert_eq!(config.oidc.redirect_uri, "http://127.0.0.1:9000/auth/callback");
    }
    #[test]
    fn an_explicit_redirect_uri_wins() {
        let scratch = Scratch::new();
        scratch.write(
            r#"database = "in.db"
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
redirect_uri = "https://files.example.com/auth/callback"
"#,
        );
        let config = scratch.load().unwrap();
        assert_eq!(
            config.oidc.redirect_uri,
            "https://files.example.com/auth/callback"
        );
    }

    /// The links in hands out follow `listen` unless the file says a public
    /// base — the mirror of `redirect_uri`, which is why the tests read the
    /// same way.
    #[test]
    fn base_url_defaults_to_the_listen_address() {
        let scratch = Scratch::new();
        scratch.write(FULL);
        let config = scratch.load().unwrap();
        assert_eq!(config.base_url, None);
        assert_eq!(config.public_origin(), "http://127.0.0.1:7655");
        // The file is completed with the key at its empty default, so a
        // reader learns the key exists.
        let completed = std::fs::read_to_string(scratch.dir.join(FILE_NAME)).unwrap();
        assert!(completed.contains("base_url = \"\""), "{completed}");
    }

    #[test]
    fn an_explicit_base_url_wins() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("[oidc]", "base_url = \"https://files.example.com\"\n[oidc]"));
        let config = scratch.load().unwrap();
        assert_eq!(config.base_url.as_deref(), Some("https://files.example.com"));
        assert_eq!(config.public_origin(), "https://files.example.com");
    }

    #[test]
    fn an_empty_base_url_is_the_listen_fallback() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("[oidc]", "base_url = \"\"\n[oidc]"));
        let config = scratch.load().unwrap();
        assert_eq!(config.base_url, None);
        assert_eq!(config.public_origin(), "http://127.0.0.1:7655");
    }

    #[test]
    fn a_base_url_without_a_scheme_is_refused() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("[oidc]", "base_url = \"files.example.com\"\n[oidc]"));
        assert!(matches!(
            scratch.load().unwrap_err(),
            ConfigError::Invalid { key: "base_url", .. }
        ));
    }

    #[test]
    fn a_base_url_with_a_trailing_slash_is_refused() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("[oidc]", "base_url = \"https://files.example.com/\"\n[oidc]"));
        assert!(matches!(
            scratch.load().unwrap_err(),
            ConfigError::Invalid { key: "base_url", .. }
        ));
    }

    #[test]
    fn zero_live_seconds_is_refused() {
        let scratch = Scratch::new();
        scratch.write(&FULL.replace("live_seconds = 300", "live_seconds = 0"));
        assert!(matches!(
            scratch.load().unwrap_err(),
            ConfigError::Invalid { key: "live_seconds", .. }
        ));
    }

    #[test]
    fn a_missing_database_is_refused() {
        let scratch = Scratch::new();
        scratch.write(
            r#"[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
"#,
        );
        assert_eq!(scratch.load().unwrap_err(), ConfigError::Missing("database"));
    }

    #[test]
    fn unknown_keys_are_ignored_and_reported() {
        let scratch = Scratch::new();
        scratch.write(
            r#"database = "in.db"
typo_key = 1
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
oidc_typo = 1
"#,
        );
        let config = scratch.load().unwrap();
        assert!(config.ignored.contains(&"typo_key".to_string()));
        assert!(config.ignored.contains(&"oidc.oidc_typo".to_string()));
        assert!(config.report().iter().any(|line| line.contains("typo_key")));
    }

    #[test]
    fn a_silent_file_is_completed_with_top_level_defaults() {
        let scratch = Scratch::new();
        scratch.write(
            r#"database = "in.db"
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
"#,
        );
        let config = scratch.load().unwrap();
        assert_eq!(config.live_seconds, 300);
        let completed = std::fs::read_to_string(scratch.dir.join(FILE_NAME)).unwrap();
        // Completed top-level keys land before the [oidc] header, never
        // inside it.
        let oidc_at = completed.find("[oidc]").unwrap();
        let listen_at = completed.find("listen = ").unwrap();
        assert!(listen_at < oidc_at);
        let reparsed: toml::Value = toml::from_str(&completed).unwrap();
        assert!(reparsed.get("listen").is_some());
    }

    #[test]
    fn storage_defaults_beside_the_database() {
        let scratch = Scratch::new();
        scratch.write(FULL);
        let config = scratch.load().unwrap();
        assert_eq!(
            config.storage,
            PathBuf::from(&config.database)
                .parent()
                .unwrap()
                .join("storage")
        );
    }

    #[test]
    fn storage_r2_absent_means_local() {
        let scratch = Scratch::new();
        scratch.write(FULL);
        let config = scratch.load().unwrap();
        assert_eq!(config.r2, None);
        assert!(config.report().iter().any(|line| line == "blobs     local"));
    }

    #[test]
    fn storage_r2_table_parses_and_resolves_the_key_file() {
        let scratch = Scratch::new();
        scratch.write(
            r#"database = "in.db"
[storage.r2]
account_id = "abc123"
bucket = "in-blobs"
key_file = "r2.keys"
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
"#,
        );
        let config = scratch.load().unwrap();
        let r2 = config.r2.clone().expect("[storage.r2] set");
        assert_eq!(r2.account_id, "abc123");
        assert_eq!(r2.bucket, "in-blobs");
        assert_eq!(r2.key_file, scratch.dir.join("r2.keys"));
        assert!(config
            .report()
            .iter()
            .any(|line| line == "blobs     r2 in-blobs"));
        // The storage directory still resolves — chunks stage there — and
        // the file is completed with no top-level `storage =` line, which
        // would collide with the table on the next parse.
        assert_eq!(
            config.storage,
            PathBuf::from(&config.database)
                .parent()
                .unwrap()
                .join("storage")
        );
        let completed = std::fs::read_to_string(scratch.dir.join(FILE_NAME)).unwrap();
        assert!(!mentions(&completed, "storage"), "{completed}");
    }

    #[test]
    fn an_incomplete_storage_r2_table_is_refused() {
        let scratch = Scratch::new();
        scratch.write(
            r#"database = "in.db"
[storage.r2]
account_id = "abc123"
[oidc]
issuer = "https://id.example.com"
client_id = "in"
client_secret = "s3cret"
"#,
        );
        assert_eq!(
            scratch.load().unwrap_err(),
            ConfigError::Missing("storage.r2.bucket")
        );
    }
}
