use std::sync::Arc;

use in_core::Config;
use in_core::store::TursoStore;
use topcoat::Result;
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{BodyLimit, Router, RouterBuilderDiscoverExt, route};

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
    // empty database, it is a second In writing a different file while
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
    // `open` applies any unapplied migration before it returns.
    let store = TursoStore::open(&config.database, Some(config.storage.as_path()))
        .await
        .expect("failed to open the database");
    let store: Arc<dyn in_core::store::Store> = Arc::new(store);

    // Trash purge is policy, not storage hygiene, so it happens here with the
    // configured horizon rather than inside `open`: a deployment keeping trash
    // longer than any built-in default must never be over-purged by boot.
    let cutoff =
        time::OffsetDateTime::now_utc() - time::Duration::days(i64::from(config.purge_after_days));
    match store.purge_expired(cutoff).await {
        Ok(0) => {}
        Ok(purged) => println!("in    purged {purged} trashed item(s)"),
        Err(problem) => eprintln!("in: trash purge failed: {problem}"),
    }

    // The key sealing the OIDC session cookies, kept beside the database as
    // `in.key` — one key per deployment, never in the repository.
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
    };

    // Told when the process is stopping, so the live streams end instead of
    // being waited out. See `in_web::live::Shutdown`.
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let live_seconds = config.live_seconds;
    let listen = config.listen;

    let router = in_client::mount(
        Router::builder()
            .discover()
            .layer(BodyLimit::max(16 * 1024 * 1024).at("/api/upload"))
            .layer(BodyLimit::max(64 * 1024 * 1024).at("/files"))
            .cookies()
            .assets(bundle),
        oidc,
    )
    .app_context(in_web::server::App {
        store,
        config,
        shutdown: in_web::live::Shutdown(stopping),
    })
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
/// private to the user the process runs as. A directory that exists is left
/// exactly as it is; one that cannot be made stops the boot — the failure
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
