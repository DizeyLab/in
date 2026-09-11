use std::sync::Arc;

use in_core::Config;
use in_core::store::r2::R2Blobs;
use in_core::store::{Store, TursoStore};
use topcoat::Result;
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::context::Cx;
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{BodyLimit, Router, RouterBuilderDiscoverExt, route};
use topcoat::runtime::RouterBuilderRuntimeExt;

#[route(GET "/healthz")]
async fn healthz() -> Result<&'static str> {
    // The deploy asserts this against the commit it pushed, so a stale
    // process still holding the port fails the deploy instead of
    // answering a green health check.
    Ok(concat!("ok ", env!("IN_BUILD_SHA")))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("reconcile") {
        let mut dry_run = false;
        let mut yes = false;
        for arg in &args[2..] {
            match arg.as_str() {
                "--dry-run" => dry_run = true,
                "--yes" => yes = true,
                _ => {
                    eprintln!("in reconcile: unknown option {arg}");
                    std::process::exit(2);
                }
            }
        }
        let config = match Config::load() {
            Ok(config) => config,
            Err(problem) => {
                eprintln!("in: {problem}");
                std::process::exit(2);
            }
        };
        if let Err(problem) = in_core::store::reconcile(
            &config.database,
            Some(config.storage.as_path()),
            in_core::store::ReconcileOptions {
                dry_run,
                yes,
                auto: false,
            },
        )
        .await
        {
            eprintln!("in reconcile: {problem}");
            std::process::exit(1);
        }
        return;
    }

    // config/in.toml is read here, before anything is opened, and written with
    // development defaults if it is not there yet. A broken key stops the
    // boot with its name in the message: the failure this prevents is not an
    // empty database, it is a second in writing a different file while
    // everyone believes they share a drive.
    let config = match Config::load() {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("in: {problem}");
            std::process::exit(2);
        }
    };
    // Said once, so the answer to "which file are we on" lives in the log.
    println!("in    database {}", config.database);
    println!("in    storage {}", config.storage.display());
    println!("in    listen {}", config.listen);

    // The bundle beside the executable is the only stylesheet this process
    // can serve, and nothing in topcoat binds it to this binary's
    // generation: a bundle left behind by another deploy loads as happily
    // as the right one, and the pages then reference a stylesheet whose
    // bytes are days old — the mixed generation a browser once caught on
    // production. The fingerprint build.rs stamped into this binary is
    // checked against the bundle's bytes, and a foreign bundle refuses the
    // boot rather than serving under it.
    let bundle = AssetBundle::load().unwrap_or_else(|err| {
        eprintln!("in: the asset bundle beside the executable failed to load: {err}");
        std::process::exit(2);
    });
    let stylesheet = match in_web::server::stylesheet_guard(&bundle) {
        Ok(line) => line,
        Err(problem) => {
            eprintln!("in: {problem}");
            std::process::exit(2);
        }
    };
    println!("in    {stylesheet}");

    // File bytes, thumbnails and staged chunks live beside the database, not
    // in a table. The tree is made before the store opens, because the
    // reconcile an old database triggers on the way needs somewhere to put
    // what it finds.
    ensure_storage_tree(&config.storage);

    // One process per database file: Turso is a single-writer engine and a
    // second process on the same file loses writes rather than queueing.
    //
    // `open` applies any unapplied migration before it returns. With
    // `[storage.r2]` set, the bytes open against the bucket instead of the
    // tree — the tree keeps staging chunks, the bucket holds `files/` and
    // `thumbs/`. No `[storage.r2]` is the local backend, today's shape.
    let store = match &config.r2 {
        Some(r2) => {
            // Two lines: the access key id, then the secret. The file is
            // the deployment's, outside the repository; an unreadable one
            // stops the boot here rather than letting every write fail
            // later with the same message.
            let keys = match std::fs::read_to_string(&r2.key_file) {
                Ok(keys) => keys,
                Err(problem) => {
                    eprintln!("in: could not read {}: {problem}", r2.key_file.display());
                    std::process::exit(2);
                }
            };
            let mut lines = keys.lines().map(str::trim).filter(|line| !line.is_empty());
            let access_key_id = lines.next().unwrap_or_default();
            let secret_access_key = lines.next().unwrap_or_default();
            if access_key_id.is_empty() || secret_access_key.is_empty() {
                eprintln!(
                    "in: {} must carry the access key id on its first line and the secret on its second",
                    r2.key_file.display()
                );
                std::process::exit(2);
            }
            let endpoint = format!("https://{}.r2.cloudflarestorage.com", r2.account_id);
            let blobs = match R2Blobs::open(
                &endpoint,
                &r2.bucket,
                access_key_id,
                secret_access_key,
                false,
            ) {
                Ok(blobs) => blobs,
                Err(problem) => {
                    eprintln!("in: could not open the r2 bucket {}: {problem}", r2.bucket);
                    std::process::exit(2);
                }
            };
            eprintln!("in    blobs r2 {}", r2.bucket);
            TursoStore::open_with_blobs(
                &config.database,
                Some(config.storage.as_path()),
                Arc::new(blobs),
            )
            .await
            .expect("failed to open the database")
        }
        None => TursoStore::open(&config.database, Some(config.storage.as_path()))
            .await
            .expect("failed to open the database"),
    };
    let store = Arc::new(store);

    // Trash purge is policy, not storage hygiene: it happens with the
    // configured horizon rather than any built-in default, so a deployment
    // keeping trash longer than the default is never over-purged.
    let cutoff =
        time::OffsetDateTime::now_utc() - time::Duration::days(i64::from(config.purge_after_days));

    // Nothing the first request needs waits for the boot's hygiene: the
    // walk of every file and thumbnail on disk, the aborted uploads, the
    // trash purge. The store is usable the moment `open` returns — schema
    // live, storage tree in place — and the sweeps only delete what no row
    // names and what has already expired, so they run as a background task
    // while the port is about to answer. A sweep that fails is logged and
    // lived with: an unswept tree is slow, never wrong.
    let sweeper = store.clone();
    tokio::spawn(async move {
        if let Err(problem) = sweeper.boot_sweeps().await {
            eprintln!("in: boot sweeps failed: {problem}");
        }
        match sweeper.purge_expired(cutoff).await {
            Ok(0) => {}
            Ok(purged) => println!("in    purged {purged} trashed item(s)"),
            Err(problem) => eprintln!("in: trash purge failed: {problem}"),
        }
    });
    let store: Arc<dyn in_core::store::Store> = store;

    // The `in.key` loaded here seals two things: the OIDC session cookies
    // below, and — through the app context — the share-link tokens their
    // creation seals onto setting rows, so the full link addresses can be
    // re-shown later. One key per deployment, never in the repository.
    let key_path = std::path::Path::new(&config.database)
        .parent()
        .map(|parent| parent.join("in.key"))
        .unwrap_or_else(|| std::path::PathBuf::from("in.key"));
    let cookie_key = match in_core::store::secret::load_or_create_key(&key_path) {
        Ok(key) => key,
        Err(problem) => {
            eprintln!("in: could not load {}: {problem}", key_path.display());
            std::process::exit(2);
        }
    };
    let oidc = in_client::Config {
        issuer: config.oidc.issuer.clone(),
        client_id: config.oidc.client_id.clone(),
        client_secret: config.oidc.client_secret.clone(),
        redirect_uri: config.oidc.redirect_uri.clone(),
        cookie_name: "in_session".to_string(),
        cookie_key,
        // Where im's `/logout` sends the browser after the central
        // sign-out: the same origin public links carry.
        logout_back: config.public_origin(),
    };

    // The same client pair, pointed at im's identity directory: the roster
    // the mirror below files, the photo bytes the avatar route proxies, and
    // the live feed that makes both arrive without waiting for a beat.
    let directory = im_client::directory::DirectoryClient::new(
        config.oidc.issuer.clone(),
        config.oidc.client_id.clone(),
        config.oidc.client_secret.clone(),
    );
    let default_quota_bytes = config.default_quota_bytes;
    let health = in_web::server::DirectoryHealth::new();
    tokio::spawn(identity_sync(
        store.clone(),
        directory.clone(),
        default_quota_bytes,
        health.clone(),
    ));

    // The suite the switcher's wordmarks link to is im's to keep: im's
    // admin panel owns the list and `/family` serves it to registered
    // apps. This mirror is refreshed on the beat, first pass right away so
    // a fresh deploy shows the trio at boot; an im that does not answer —
    // down, restarting, mid-deploy — costs one log line and nothing else:
    // the last list filed keeps the switcher alive until the next beat.
    // The address the app registers itself under: `base_url` alone, without
    // the trailing slash im keeps its entries free of. The bound address is
    // no fallback here — a loopback bind is not an address anyone else can
    // reach, and filing it would overwrite the real one in every switcher.
    let public_origin = config
        .base_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    tokio::spawn(family_sync(
        store.clone(),
        in_client::InClient::new(oidc.clone()),
        public_origin,
        default_quota_bytes,
    ));

    // Told when the process is stopping, so the live streams end instead of
    // being waited out. See `in_web::live::Shutdown`.
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let live_seconds = config.live_seconds;
    let listen = config.listen;

    let router = in_client::mount(
        Router::builder()
            .discover()
            .runtime()
            // BodyLimit is only a memory guard here, not the upload policy:
            // the account quota, enforced in the store at start and finish,
            // is the only size limit. `/files` takes whole multipart bodies,
            // so its guard sits at a hard 2 GiB; `/api/upload` carries single
            // 8 MiB chunks, so 32 MiB leaves ample headroom.
            .layer(BodyLimit::max(32 * 1024 * 1024).at("/api/upload"))
            .layer(BodyLimit::max(512 * 1024 * 1024).at("/api/service/files"))
            .layer(BodyLimit::max(2usize * 1024 * 1024 * 1024).at("/files"))
            .cookies()
            .assets(bundle),
        oidc,
    )
    .app_context(in_web::server::App {
        store,
        config,
        shutdown: in_web::live::Shutdown(stopping),
        link_key: cookie_key,
        directory,
        health,
    })
    .app_context(in_client::LogoutBack(Arc::new(|cx: &Cx| {
        Box::pin(async move { Some(in_web::server::share_origin(cx).await) })
    })))
    .app_context(in_web::live::LiveWindow(std::time::Duration::from_secs(
        live_seconds,
    )))
    .build();

    // `topcoat::start` binds HOST/PORT from the environment; the listen
    // address is a config/in.toml decision, so the listener is bound
    // explicitly against the same value the boot log just printed.
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("failed to bind the listen address");
    // Not `topcoat::serve`, which installs its own signal handler and gives
    // no way to hear it. The handler is taken over so the live streams learn
    // about the stop before the graceful shutdown starts counting: without
    // that, every open tab holds a stream the shutdown waits its full thirty
    // seconds for, and Ctrl+C appears to hang.
    topcoat::serve_until(listener, router, async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    })
    .await
    .expect("server error");
}

/// Makes the storage tree the store keeps binary files in, if it is not
/// there: `<storage>/files`, `<storage>/thumbs` and `<storage>/uploads`,
/// private to the user the process runs as. A
/// directory that exists is left exactly as it is; one that cannot be made
/// stops the boot — the failure
/// this prevents is an upload landing in a tree that is not there, and it is
/// better met before anything is opened.
fn ensure_storage_tree(storage: &std::path::Path) {
    let make = |dir: &std::path::Path| {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!("in: could not create {}: {err}", dir.display());
            std::process::exit(2);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            {
                eprintln!("in: could not restrict {}: {err}", dir.display());
                std::process::exit(2);
            }
        }
    };
    make(storage);
    for name in ["files", "thumbs", "uploads"] {
        make(&storage.join(name));
    }
}

/// Resolves when the process is asked to stop: Ctrl+C, or `SIGTERM` from a
/// service manager.
async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl+C handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

/// The longest wait a run of directory failures earns before trying again.
const IDENTITY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Mirrors one member's identity into the local row: the JIT upsert that
/// keeps address, name, admin flag and photo stamp following im — and the
/// disablement flag with it, both directions, for the row is im's word on
/// the person and a re-enable must land here as surely as a kill. The
/// store announces a `profile` change itself when a mirrored field moved,
/// and a flipped disablement announces the admin surface, the profile and
/// the person's own tabs all at once, so the only job left here is the
/// write and its one log line when it fails.
async fn mirror_member(
    store: &Arc<dyn in_core::store::Store>,
    member: &im_client::directory::DirectoryMember,
    default_quota_bytes: u64,
) {
    let user = match store
        .provision_user(
            &member.sub,
            &member.email,
            &member.name,
            Some(member.admin),
            member.photo_version,
            default_quota_bytes,
        )
        .await
    {
        Ok(user) => user,
        Err(problem) => {
            eprintln!("in: directory mirror of {}: {problem}", member.sub);
            return;
        }
    };
    if user.disabled != member.disabled
        && let Err(problem) = store.set_user_disabled(&user.id, member.disabled).await
    {
        eprintln!(
            "in: directory disablement mirror of {}: {problem}",
            member.sub
        );
    }
}

/// Keeps the local user rows a live mirror of im's identity directory.
///
/// The shape is one pass, one stream, repeat: a full `/directory` pass
/// files (or heals) every member row, then `/directory/live` carries each
/// change as it happens. When the stream ends — im restarting, or its
/// window closing — the loop simply runs the full pass again: a lagged
/// reader hears silence, not partial frames, so the re-list is what catches
/// it up, and the redial is the fast path again. A pass that fails backs
/// off a second, then doubles to the cap, resetting on the next answer.
/// Runs the whole life of the process; nothing here is worth keeping alive
/// past shutdown.
async fn identity_sync(
    store: Arc<dyn in_core::store::Store>,
    client: im_client::directory::DirectoryClient,
    default_quota_bytes: u64,
    health: in_web::server::DirectoryHealth,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match client.directory().await {
            Ok(members) => {
                backoff = std::time::Duration::from_secs(1);
                for member in &members {
                    mirror_member(&store, member, default_quota_bytes).await;
                }
                // The roster pass committed: the card's healer heartbeat.
                health.note_pass();
                match client.open_stream().await {
                    Ok(mut stream) => {
                        health.connected();
                        while let Some(event) = stream.next().await {
                            match event {
                                Ok(event) => {
                                    apply_directory_event(
                                        &store,
                                        &health,
                                        event,
                                        default_quota_bytes,
                                    )
                                    .await;
                                }
                                // A network failure ends the stream; the full
                                // pass below re-lists before the redial.
                                Err(problem) => {
                                    health.reconnecting();
                                    eprintln!("in: directory stream failed: {problem}");
                                    break;
                                }
                            }
                        }
                        // The stream closed cleanly — im restarting or its
                        // window running out. The card goes amber until the
                        // redial below opens the next one.
                        health.reconnecting();
                    }
                    Err(problem) => {
                        health.reconnecting();
                        eprintln!("in: directory stream refused: {problem}")
                    }
                }
            }
            Err(problem) => {
                health.reconnecting();
                eprintln!("in: directory pass failed: {problem}")
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(IDENTITY_BACKOFF_CAP);
    }
}

/// One event off the directory stream, applied to the local mirror. The
/// seam exists so the tests can drive a constructed event through the
/// same door identity_sync's stream loop uses — the loop itself only
/// knows how to read the wire.
async fn apply_directory_event(
    store: &Arc<dyn in_core::store::Store>,
    health: &in_web::server::DirectoryHealth,
    event: im_client::directory::DirectoryEvent,
    default_quota_bytes: u64,
) {
    match event {
        im_client::directory::DirectoryEvent::Profile(member) => {
            mirror_member(store, &member, default_quota_bytes).await;
        }
        // Sessions died at im; no local row moved. Wake the person
        // through the profile topic — their tabs refetch, meet the dead
        // introspection, and morph to the sign-in card.
        im_client::directory::DirectoryEvent::Revoked { sub } => {
            announce_revocation(store, &sub).await;
        }
    }
    // Every event is proof the feed is alive; the card's age line reads
    // this.
    health.note_event();
}

/// The session-targeted half of a directory revocation: im revoked the
/// person's sessions and no local row moved, but every tab carrying them
/// must find out. The row is woken through the ordinary profile
/// announcement — the one topic every signed-in reader may hear — so the
/// revoked person's own tabs refetch, meet a dead introspection, and
/// morph to the sign-in card: the whole of what a session-level logout
/// looks like from here. No hard redirect: which of the person's
/// sessions died is im's knowledge alone, and a surviving tab must not
/// be ordered out for its sibling's death.
async fn announce_revocation(store: &Arc<dyn in_core::store::Store>, sub: &str) {
    match store.user_by_oidc_sub(sub).await {
        Ok(Some(user)) => store.announce_profile(&user.id),
        // Nobody local carries this subject: nobody here to wake.
        Ok(None) => {}
        Err(problem) => eprintln!("in: revocation wake for {sub}: {problem}"),
    }
}

/// How often the suite list is re-fetched from im. Short enough that a
/// service the admin adds over there shows up here before anyone goes
/// looking for its wordmark, long enough that the two are not talking
/// about it constantly.
const FAMILY_SECONDS: u64 = 300;

/// Keeps the switcher's list a mirror of im's family: fetched as this app
/// (Basic client credentials) and filed as a JSON string under the
/// `family` setting — the one row the switcher reads, so a beat that
/// answers simply replaces what is filed. Runs the whole life of the
/// process; nothing here is worth keeping alive past shutdown.
async fn family_sync(
    store: Arc<dyn in_core::store::Store>,
    client: in_client::InClient,
    origin: String,
    default_quota_bytes: u64,
) {
    if origin.is_empty() {
        eprintln!("family: no public address to register; set base_url");
    }
    loop {
        // Announced before every fetch, so an im that was down, restarted
        // or freshly seeded learns this app on the next beat. A refusal is
        // one log line: the mirror below runs either way.
        if !origin.is_empty()
            && let Err(problem) = client.register_family("in", "Files", &origin).await
        {
            eprintln!("family register: {problem}");
        }
        match client.family().await {
            Some(family) => {
                // The limits ride the same list: every service key's account
                // takes the ceiling im states for it (the house default when
                // im states none). A refused write is one log line — the
                // next beat carries the same word.
                if let Err(problem) =
                    in_web::service::apply_family_limits(&store, &family, default_quota_bytes).await
                {
                    eprintln!("family limits: {problem}");
                }
                match serde_json::to_string(&family) {
                    Ok(json) => {
                        if let Err(problem) = store.set_setting("family", &json).await {
                            eprintln!("family sync: {problem}");
                        }
                    }
                    Err(problem) => eprintln!("family sync: {problem}"),
                }
            }
            None => eprintln!("family sync: im did not answer; keeping the list there is"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(FAMILY_SECONDS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use in_core::store::Store;

    /// A revoked session at im wakes the person's row by its local id:
    /// one profile announcement, heard by every signed-in reader, with
    /// the person's own tabs morphing to the sign-in card on the refetch.
    /// A subject that never signed in here wakes nobody.
    #[tokio::test]
    async fn a_revoked_session_wakes_the_persons_row_through_the_profile_topic() {
        let dir = std::env::temp_dir().join(format!("in-revoke-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn in_core::store::Store> = Arc::new(
            in_core::store::TursoStore::open(
                dir.join("in.db").to_str().unwrap(),
                Some(&dir.join("storage")),
            )
            .await
            .unwrap(),
        );
        let user = store
            .provision_user("sub-gone", "gone@in.test", "Gone", None, 0, 1024)
            .await
            .unwrap();
        let mut rx = store.subscribe();

        announce_revocation(&store, "sub-gone").await;

        let change = rx.try_recv().unwrap();
        assert_eq!(change.topic.kind(), "profile");
        assert_eq!(change.topic.id(), user.id);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        announce_revocation(&store, "sub-unknown").await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One roster member as the wire carries it.
    fn member(sub: &str, disabled: bool) -> im_client::directory::DirectoryMember {
        im_client::directory::DirectoryMember {
            sub: sub.to_string(),
            email: format!("{sub}@in.test"),
            name: "Member".to_string(),
            admin: false,
            photo_version: 0,
            timezone: "UTC+03:00".to_string(),
            disabled,
        }
    }

    async fn scratch() -> (std::path::PathBuf, Arc<dyn in_core::store::Store>) {
        let dir = std::env::temp_dir().join(format!("in-event-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let store: Arc<dyn in_core::store::Store> = Arc::new(
            in_core::store::TursoStore::open(
                dir.join("in.db").to_str().unwrap(),
                Some(&dir.join("storage")),
            )
            .await
            .unwrap(),
        );
        (dir, store)
    }

    /// A `revoked` frame off the stream — im's word that the subject's
    /// sessions died — wakes the person's row by its local id through
    /// the profile topic, and stamps the connection card's freshness.
    #[tokio::test]
    async fn a_stream_revocation_announces_through_the_profile_topic() {
        let (dir, store) = scratch().await;
        let health = in_web::server::DirectoryHealth::new();
        let user = store
            .provision_user("sub-gone", "gone@in.test", "Gone", None, 0, 1024)
            .await
            .unwrap();
        let mut rx = store.subscribe();

        apply_directory_event(
            &store,
            &health,
            im_client::directory::DirectoryEvent::Revoked {
                sub: "sub-gone".to_string(),
            },
            1024,
        )
        .await;

        let change = rx.try_recv().unwrap();
        assert_eq!(change.topic.kind(), "profile");
        assert_eq!(change.topic.id(), user.id);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(health.last_event_age_secs().is_some());

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The member's disablement flag follows im in both directions: a
    /// `profile` frame carrying `disabled` flips the local row and — the
    /// part the tabs wait for — the store announces the kill (and, on
    /// the way back, the reopening) on all three surfaces at once.
    #[tokio::test]
    async fn a_members_disabled_flag_mirrors_both_ways() {
        let (dir, store) = scratch().await;
        let health = in_web::server::DirectoryHealth::new();
        let mut rx = store.subscribe();

        // First sight: the mirror inserts the row, present and enabled.
        apply_directory_event(
            &store,
            &health,
            im_client::directory::DirectoryEvent::Profile(member("sub-m", false)),
            1024,
        )
        .await;
        let user = store.user_by_oidc_sub("sub-m").await.unwrap().unwrap();
        assert!(!user.disabled);
        // A fresh row announces the admin surface and the profile — the
        // two reads that keep the later phases' queues clean.
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "admin");
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "profile");
        // im switches the member off: the row follows, and the revoked
        // frame rides to the person's own tabs.
        apply_directory_event(
            &store,
            &health,
            im_client::directory::DirectoryEvent::Profile(member("sub-m", true)),
            1024,
        )
        .await;
        let killed = store.user_by_oidc_sub("sub-m").await.unwrap().unwrap();
        assert!(killed.disabled);
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "admin");
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "profile");
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "revoked");

        // im lets them back in: the same channel carries the reopening.
        apply_directory_event(
            &store,
            &health,
            im_client::directory::DirectoryEvent::Profile(member("sub-m", false)),
            1024,
        )
        .await;
        let back = store.user_by_oidc_sub("sub-m").await.unwrap().unwrap();
        assert!(!back.disabled);
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "admin");
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "profile");
        assert_eq!(rx.try_recv().unwrap().topic.kind(), "revoked");
        assert!(health.last_event_age_secs().is_some());

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
