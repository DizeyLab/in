//! The drive driven the way a browser drives it: real router, real store,
//! real session cookie, fake im.
//!
//! Auth in tests: the OIDC dance cannot run against a live im, so each test
//! spins a fake one — a bare TCP server answering `POST /introspect` — and
//! signs users in with `in_client::mint_session_cookie` (feature
//! `test-seam`). The token<->claims map is per-test, so one test's session
//! is meaningless to the next.
//!
//! New HTTP tests belong in this file rather than a new `tests/*.rs`: one
//! test binary links and runs once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use http::{HeaderValue, Request, StatusCode, header};
use in_core::store::{ShareKind, Store, TursoStore};
use in_core::{Config, OidcConfig};
use in_web::server::App;
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::RouterBuilderCookieExt;
use topcoat::router::{Body, BodyLimit, Router, RouterBuilderDiscoverExt, to_bytes};
use ulid::Ulid;

/// The bundle `cargo build -p in-web` + `topcoat asset bundle --bin in-web`
/// write next to the crate's own `target/debug`, not next to the test
/// binary (which lives in `target/debug/deps`) — `AssetBundle::load` looks
/// beside `current_exe` and would miss it, so the path is given explicitly
/// instead.
fn asset_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug/assets"
    ))
}

/// A fake im: answers `POST /introspect` from its token map, `{"active":
/// false}` for anything it does not know. Bare TCP + hand-rolled HTTP/1.1 —
/// just enough for the client's form post.
struct FakeIm {
    addr: std::net::SocketAddr,
    tokens: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    photos: Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>,
}

impl FakeIm {
    async fn spawn() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tokens: Arc<Mutex<HashMap<String, serde_json::Value>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let photos: Arc<Mutex<HashMap<String, (Vec<u8>, String)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let map = tokens.clone();
        let pmap = photos.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let map = map.clone();
                let pmap = pmap.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut socket = socket;
                    let mut buf = vec![0u8; 8192];
                    let mut req = Vec::new();
                    // One request after another on the same connection:
                    // closing it after every answer races the client's
                    // pool, which may hand out the dead socket for the
                    // next introspection and read that as signed-out.
                    loop {
                        let body_start = loop {
                            let Ok(n) = socket.read(&mut buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            req.extend_from_slice(&buf[..n]);
                            if let Some(end) = headers_end(&req) {
                                let head = String::from_utf8_lossy(&req[..end]).to_string();
                                let len = content_length(&head);
                                if req.len() >= end + len {
                                    break end;
                                }
                            }
                            if req.len() > 1_000_000 {
                                return;
                            }
                        };
                        let head = String::from_utf8_lossy(&req[..body_start]).to_string();
                        let first = head.lines().next().unwrap_or("").to_string();
                        // The body may already hold the next pipelined
                        // request's first bytes; take exactly this one.
                        let len = content_length(&head);
                        let body: Vec<u8> = req[body_start..body_start + len].to_vec();
                        req.drain(..body_start + len);
                        let (status, content_type, payload) =
                            match photo_answer(&pmap, &head, &first) {
                                Some(photo) => photo,
                                None => match answer_for(&map, &first, &body) {
                                    Some(answer) => (
                                        "200 OK",
                                        "application/json".to_string(),
                                        serde_json::to_vec(&answer).unwrap(),
                                    ),
                                    // Anything but the introspection route is
                                    // nothing at all, the way the real im 404s
                                    // unknown paths rather than answering them.
                                    None => ("404 Not Found", "text/plain".to_string(), Vec::new()),
                                },
                            };
                        let response = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n",
                            payload.len()
                        );
                        if socket.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        if socket.write_all(&payload).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            addr,
            tokens,
            photos,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stores the photo `GET /photo/{user_id}` answers with — the bytes and
    /// the mime im would serve for this account.
    fn set_photo(&self, user_id: &str, bytes: Vec<u8>, mime: &str) {
        self.photos
            .lock()
            .unwrap()
            .insert(user_id.to_string(), (bytes, mime.to_string()));
    }
}

/// The Basic credential the app presents for `GET /photo/*`: `in-test:s3cr3t`.
const PHOTO_BASIC: &str = "Basic aW4tdGVzdDpzM2NyM3Q=";

/// Answers `GET /photo/{id}` from the photo map — the app's Basic credential
/// or nothing, a missing photo exactly like a missing person. `None` for
/// anything else, which stays the introspection JSON.
fn photo_answer(
    photos: &Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>,
    head: &str,
    request_line: &str,
) -> Option<(&'static str, String, Vec<u8>)> {
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return None;
    }
    let target = parts.next().unwrap_or("");
    let path = target
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(target);
    let id = path.strip_prefix("/photo/")?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    let authed = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("authorization") && value.trim() == PHOTO_BASIC
        });
    if !authed {
        return Some(("404 Not Found", "text/plain".to_string(), Vec::new()));
    }
    match photos.lock().unwrap().get(id) {
        Some((bytes, mime)) => Some(("200 OK", mime.clone(), bytes.clone())),
        None => Some(("404 Not Found", "text/plain".to_string(), Vec::new())),
    }
}

fn headers_end(req: &[u8]) -> Option<usize> {
    req.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim().eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// Percent-decoding, enough for the opaque tokens these tests mint.
fn decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut bytes = raw.as_bytes().iter();
    while let Some(&b) = bytes.next() {
        if b == b'+' {
            out.push(' ');
        } else if b == b'%' {
            let hi = bytes.next().copied().unwrap_or(b'0');
            let lo = bytes.next().copied().unwrap_or(b'0');
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push((hex(hi) * 16 + hex(lo)) as char);
        } else {
            out.push(b as char);
        }
    }
    out
}
fn answer_for(
    map: &Arc<Mutex<HashMap<String, serde_json::Value>>>,
    request_line: &str,
    body: &[u8],
) -> Option<serde_json::Value> {
    let mut head = request_line.split_whitespace();
    let is_introspect =
        head.next() == Some("POST") && head.next() == Some(in_client::introspect_path());
    if !is_introspect {
        return None;
    }
    let text = String::from_utf8_lossy(body).to_string();
    let token = text
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "token")
        .map(|(_, value)| decode(value));
    match token.and_then(|token| map.lock().unwrap().get(&token).cloned()) {
        Some(claims) => Some(claims),
        None => Some(serde_json::json!({"active": false})),
    }
}

/// A throwaway workspace: its own database file, its own fake im, its own
/// router.
struct TestApp {
    dir: PathBuf,
    router: Router,
    store: Arc<dyn Store>,
    config: Config,
    client: in_client::Config,
    fake: FakeIm,
    stop: tokio::sync::watch::Sender<bool>,
}

impl TestApp {
    async fn build() -> Self {
        let dir = std::env::temp_dir().join(format!("in-http-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("in.db");
        let storage = dir.join("storage");
        let store: Arc<dyn Store> = Arc::new(
            TursoStore::open(db.to_str().unwrap(), Some(storage.as_path()))
                .await
                .unwrap(),
        );
        let fake = FakeIm::spawn().await;
        let client = in_client::Config {
            issuer: fake.url(),
            client_id: "in-test".to_string(),
            client_secret: "s3cr3t".to_string(),
            redirect_uri: "http://127.0.0.1:7655/auth/callback".to_string(),
            cookie_name: "in_session".to_string(),
            cookie_key: [7u8; 32],
        };
        let config = Config {
            database: db.to_str().unwrap().to_string(),
            storage,
            listen: "127.0.0.1:7655".parse().unwrap(),
            live_seconds: 300,
            purge_after_days: 30,
            default_quota_bytes: 10 * 1024 * 1024 * 1024,
            oidc: OidcConfig {
                issuer: fake.url(),
                client_id: "in-test".to_string(),
                client_secret: "s3cr3t".to_string(),
                redirect_uri: "http://127.0.0.1:7655/auth/callback".to_string(),
            },
            ignored: Vec::new(),
            defaulted: false,
        };
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let router = in_client::mount(
            Router::builder()
                .discover()
                .layer(BodyLimit::max(32 * 1024 * 1024).at("/api/upload"))
                .layer(BodyLimit::max(2usize * 1024 * 1024 * 1024).at("/files"))
                .cookies()
                .assets(
                    AssetBundle::load_dir(asset_dir())
                        .expect("run `topcoat asset bundle` before the http suite"),
                ),
            client.clone(),
        )
        .app_context(App {
            store: store.clone(),
            config: config.clone(),
            shutdown: in_web::live::Shutdown(stopping),
        })
        .app_context(in_web::live::LiveWindow(std::time::Duration::from_secs(10)))
        .build();
        Self {
            dir,
            router,
            store,
            config,
            client,
            fake,
            stop,
        }
    }

    /// Provisions the row and mints a session for it, returning the `Cookie`
    /// header value to send.
    async fn sign_in(&self, sub: &str, email: &str, name: &str) -> String {
        let user = self
            .store
            .provision_user(sub, email, name, self.config.default_quota_bytes)
            .await
            .unwrap();
        let token = format!("tok-{}", Ulid::new());
        let exp = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        self.fake.tokens.lock().unwrap().insert(
            token.clone(),
            serde_json::json!({
                "active": true,
                "sub": user.oidc_sub,
                "email": user.email,
                "name": user.display_name,
                "exp": exp.unix_timestamp(),
            }),
        );
        let value = in_client::mint_session_cookie(&self.client, &token, exp);
        format!("in_session={value}")
    }

    /// Posts a form the way a hydrated caller does: `Accept:
    /// application/json`, no `Referer`. Every mutating `/api/*` route
    /// answers `303 See Other` regardless — [`Router::handle`] never follows
    /// a redirect, so the answer is read straight off this response.
    async fn post(&self, path: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Answer {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::from(body)).unwrap())
            .await;
        Answer::from_response(response).await
    }

    /// Posts JSON the way the upload script does.
    async fn post_json(
        &self,
        path: &str,
        cookie: Option<&str>,
        value: serde_json::Value,
    ) -> Answer {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::from(value.to_string())).unwrap())
            .await;
        Answer::from_response(response).await
    }

    /// Puts raw bytes the way the upload script sends one chunk.
    async fn put_bytes(&self, path: &str, cookie: Option<&str>, bytes: &[u8]) -> Answer {
        let mut request = Request::builder()
            .method("PUT")
            .uri(path)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::from(bytes.to_vec())).unwrap())
            .await;
        Answer::from_response(response).await
    }

    /// Posts a multipart form the way the drive's upload control does,
    /// hand-built rather than pulling in a client crate for it: the fields
    /// first, in the order given, then one `file` part per file.
    async fn post_multipart(
        &self,
        path: &str,
        cookie: Option<&str>,
        fields: &[(&str, &str)],
        files: &[(&str, &str, &[u8])],
    ) -> Answer {
        const BOUNDARY: &str = "in-test-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        for (filename, content_type, bytes) in files {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let mut request = Request::builder().method("POST").uri(path).header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::from(body)).unwrap())
            .await;
        Answer::from_response(response).await
    }

    /// Gets a page or a download the way a browser does, raw bytes back
    /// untouched — a download's body is not always UTF-8.
    async fn get(&self, path: &str, cookie: Option<&str>) -> Raw {
        self.get_with(path, cookie, None, None).await
    }

    /// Like `get`, but with a `Range` header, for the partial-content path.
    async fn get_with_range(&self, path: &str, cookie: Option<&str>, range: &str) -> Raw {
        self.get_with(path, cookie, Some(range), None).await
    }

    /// Like `get`, but with `If-None-Match`, for the thumbnail revalidate
    /// path.
    async fn get_with_if_none_match(&self, path: &str, cookie: Option<&str>, etag: &str) -> Raw {
        self.get_with(path, cookie, None, Some(etag)).await
    }

    async fn get_with(
        &self,
        path: &str,
        cookie: Option<&str>,
        range: Option<&str>,
        if_none_match: Option<&str>,
    ) -> Raw {
        let mut request = Request::builder().method("GET").uri(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        if let Some(if_none_match) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, if_none_match);
        }
        let response = self
            .router
            .handle(request.body(Body::empty()).unwrap())
            .await;
        let status = response.status();
        let headers = response.headers().clone();
        let header = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw {
            status,
            content_type: header(header::CONTENT_TYPE),
            disposition: header(header::CONTENT_DISPOSITION),
            content_range: header(header::CONTENT_RANGE),
            accept_ranges: header(header::ACCEPT_RANGES),
            cache_control: header(header::CACHE_CONTROL),
            etag: header(header::ETAG),
            location: header(header::LOCATION),
            bytes,
        }
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
/// A mutating call's answer: always a 303, carrying the refusal (or its
/// absence) as JSON — the same body a hydrated caller reads.
struct Answer {
    status: StatusCode,
    body: String,
    location: Option<String>,
}

impl Answer {
    async fn from_response(response: topcoat::router::response::Response) -> Self {
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Answer {
            status,
            location,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }

    fn refused(&self, code: &str, call: &str) -> bool {
        self.status == StatusCode::SEE_OTHER
            && self.location.as_deref().is_some_and(|location| {
                location.contains(&format!("refusal={code}"))
                    && location.contains(&format!("on={call}"))
            })
    }

    fn accepted(&self) -> bool {
        self.status == StatusCode::SEE_OTHER
            && self
                .location
                .as_deref()
                .is_some_and(|location| !location.contains("refusal="))
    }
}

/// A GET answer kept as raw bytes: a page's HTML or a download's file.
struct Raw {
    status: StatusCode,
    content_type: Option<String>,
    disposition: Option<String>,
    content_range: Option<String>,
    accept_ranges: Option<String>,
    cache_control: Option<String>,
    etag: Option<String>,
    location: Option<String>,
    bytes: Vec<u8>,
}

impl Raw {
    fn text(&self) -> String {
        String::from_utf8(self.bytes.clone()).unwrap()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.text()).unwrap()
    }
}

/// Form encoding, enough for the names these tests send.
fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The 1x1 transparent PNG: the smallest thing the thumbnailer must accept.
fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

/// The newest live folder with this name under this parent, read straight
/// off the store.
async fn folder_id(app: &TestApp, owner: &str, parent: Option<&str>, name: &str) -> String {
    app.store
        .list_children(owner, parent)
        .await
        .unwrap()
        .folders
        .iter()
        .find(|folder| folder.name == name)
        .expect("folder was not created")
        .id
        .clone()
}
/// The token off a creation redirect's `?created=` pair.
fn created_token(location: &str) -> String {
    let query = location
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "created")
        .map(|(_, value)| value.to_string())
        .expect("creation set no token")
}

async fn file_id(app: &TestApp, owner: &str, name: &str, bytes: &[u8]) -> String {
    app.store
        .insert_file(owner, None, name, bytes)
        .await
        .unwrap()
        .id
}

async fn owner_of(app: &TestApp, sub: &str) -> String {
    app.store
        .provision_user(sub, "x@y.z", "X", app.config.default_quota_bytes)
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn drive_redirects_without_session() {
    let app = TestApp::build().await;
    let page = app.get("/drive", None).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location.as_deref(), Some("/"));
}

#[tokio::test]
async fn folder_crud_and_cycle_is_refused() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-crud", "crud@in.test", "Crud").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-crud")
        .await
        .unwrap()
        .unwrap();

    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "projects")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.body, "null", "creating was refused");

    let projects = folder_id(&app, &user.id, None, "projects").await;
    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", &projects), ("name", "sub")],
        )
        .await;
    assert_eq!(answer.body, "null", "creating was refused");
    let sub = folder_id(&app, &user.id, Some(&projects), "sub").await;

    // Nesting `projects` inside its own descendant is refused, not looped.
    let answer = app
        .post(
            "/api/folder/move",
            Some(&cookie),
            &[("id", &projects), ("parent_id", &sub)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert!(
        answer.body.contains("Forbidden"),
        "cycle was not refused: {}",
        answer.body
    );

    // A legal move, a rename and a delete all land clean.
    let answer = app
        .post(
            "/api/folder/move",
            Some(&cookie),
            &[("id", &sub), ("parent_id", "")],
        )
        .await;
    assert_eq!(answer.body, "null", "moving was refused: {}", answer.body);
    let answer = app
        .post(
            "/api/folder/rename",
            Some(&cookie),
            &[("id", &sub), ("name", "moved")],
        )
        .await;
    assert_eq!(answer.body, "null", "renaming was refused: {}", answer.body);
    let answer = app
        .post("/api/folder/delete", Some(&cookie), &[("id", &sub)])
        .await;
    assert_eq!(answer.body, "null", "deleting was refused: {}", answer.body);

    // The drive page names what is left.
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("projects"), "drive hides the folder");
}

#[tokio::test]
async fn small_upload_round_trip() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-small", "small@in.test", "Small").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-small")
        .await
        .unwrap()
        .unwrap();

    let bytes = b"hello in";
    let answer = app
        .post_multipart(
            "/files",
            Some(&cookie),
            &[("folder_id", "")],
            &[("hello.txt", "text/plain", bytes)],
        )
        .await;
    assert_eq!(
        answer.status,
        StatusCode::SEE_OTHER,
        "{}",
        answer.location.unwrap_or_default()
    );
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or("")
            .contains("refusal="),
        "upload was refused: {}",
        answer.location.unwrap_or_default()
    );

    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.name == "hello.txt")
        .expect("file was not stored");
    assert_eq!(file.mime, "text/plain");

    let got = app.get(&format!("/file/{}", file.id), Some(&cookie)).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, bytes);
    assert_eq!(got.accept_ranges.as_deref(), Some("bytes"));
    assert!(
        got.cache_control
            .as_deref()
            .unwrap_or("")
            .contains("immutable"),
        "missing immutable cache directive"
    );
}

#[tokio::test]
async fn chunked_upload_round_trip() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-chunk", "chunk@in.test", "Chunk").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-chunk")
        .await
        .unwrap()
        .unwrap();

    // Two chunks: the 8 MiB server size plus a tail, so the finish assembles
    // more than one staged piece.
    let total = 8 * 1024 * 1024 + 100;
    let mut bytes = Vec::with_capacity(total);
    for i in 0..total {
        bytes.push((i % 251) as u8);
    }

    let answer = app
        .post_json(
            "/api/upload/start",
            Some(&cookie),
            serde_json::json!({"folder_id": null, "name": "big.bin", "size_bytes": total}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let started = answer.json();
    let session = started["ok"]["id"]
        .as_str()
        .expect("start was refused")
        .to_string();
    assert_eq!(started["ok"]["chunk_size"].as_u64(), Some(8 * 1024 * 1024));

    let answer = app
        .put_bytes(
            &format!("/api/upload/{session}/0"),
            Some(&cookie),
            &bytes[..8 * 1024 * 1024],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        answer.json()["ok"]["received_bytes"].as_u64(),
        Some(8 * 1024 * 1024)
    );

    let answer = app
        .put_bytes(
            &format!("/api/upload/{session}/1"),
            Some(&cookie),
            &bytes[8 * 1024 * 1024..],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        answer.json()["ok"]["received_bytes"].as_u64(),
        Some(total as u64)
    );

    let answer = app
        .post_json(
            &format!("/api/upload/{session}/finish"),
            Some(&cookie),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let file_id = answer.json()["ok"]
        .as_str()
        .expect("finish was refused")
        .to_string();

    let got = app.get(&format!("/file/{file_id}"), Some(&cookie)).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, bytes);

    let row = app.store.file(&file_id).await.unwrap().unwrap();
    assert_eq!(row.owner_id, user.id);
}

#[tokio::test]
async fn an_interrupted_upload_resumes_from_its_staged_chunks() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-resume", "resume@in.test", "Resume").await;

    // Two chunks: stage the first, ask the status what a resume may skip,
    // then land the second and finish.
    let total = 8 * 1024 * 1024 + 100;
    let mut bytes = Vec::with_capacity(total);
    for i in 0..total {
        bytes.push((i % 251) as u8);
    }
    let answer = app
        .post_json(
            "/api/upload/start",
            Some(&cookie),
            serde_json::json!({"folder_id": null, "name": "resume.bin", "size_bytes": total}),
        )
        .await;
    let session = answer.json()["ok"]["id"]
        .as_str()
        .expect("start was refused")
        .to_string();

    app.put_bytes(
        &format!("/api/upload/{session}/0"),
        Some(&cookie),
        &bytes[..8 * 1024 * 1024],
    )
    .await;

    let status = app
        .get(&format!("/api/upload/{session}"), Some(&cookie))
        .await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.text());
    let body = status.json();
    assert_eq!(body["ok"]["size_bytes"].as_u64(), Some(total as u64));
    assert_eq!(body["ok"]["chunk_size"].as_u64(), Some(8 * 1024 * 1024));
    assert_eq!(body["ok"]["name"].as_str(), Some("resume.bin"));
    assert_eq!(body["ok"]["uploaded"], serde_json::json!([0]));

    // Another owner's probe of the same id is not found, not forbidden.
    let other = app.sign_in("sub-nosey", "nosey@in.test", "Nosey").await;
    let status = app
        .get(&format!("/api/upload/{session}"), Some(&other))
        .await;
    assert_eq!(status.json()["err"].as_str(), Some("NotFound"));

    app.put_bytes(
        &format!("/api/upload/{session}/1"),
        Some(&cookie),
        &bytes[8 * 1024 * 1024..],
    )
    .await;
    let answer = app
        .post_json(
            &format!("/api/upload/{session}/finish"),
            Some(&cookie),
            serde_json::json!({}),
        )
        .await;
    let file_id = answer.json()["ok"]
        .as_str()
        .expect("finish was refused")
        .to_string();
    let got = app.get(&format!("/file/{file_id}"), Some(&cookie)).await;
    assert_eq!(got.bytes, bytes);
}
#[tokio::test]
async fn range_request_serves_a_slice() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-range", "range@in.test", "Range").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-range")
        .await
        .unwrap()
        .unwrap();

    let bytes = b"hello in";
    app.post_multipart(
        "/files",
        Some(&cookie),
        &[("folder_id", "")],
        &[("hello.txt", "text/plain", bytes)],
    )
    .await;
    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap();

    let slice = app
        .get_with_range(&format!("/file/{}", file.id), Some(&cookie), "bytes=0-4")
        .await;
    assert_eq!(slice.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(slice.bytes, b"hello");
    assert_eq!(slice.content_range.as_deref(), Some("bytes 0-4/8"));

    let past_end = app
        .get_with_range(&format!("/file/{}", file.id), Some(&cookie), "bytes=99-")
        .await;
    assert_eq!(past_end.status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn quota_is_refused_before_and_during_upload() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-quota", "quota@in.test", "Quota").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-quota")
        .await
        .unwrap()
        .unwrap();
    app.store.set_user_quota(&user.id, 10).await.unwrap();

    let answer = app
        .post_json(
            "/api/upload/start",
            Some(&cookie),
            serde_json::json!({"folder_id": null, "name": "too-big.bin", "size_bytes": 100}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        answer.json()["err"].as_str(),
        Some("QuotaExceeded"),
        "start was not refused: {}",
        answer.body
    );

    let answer = app
        .post_multipart(
            "/files",
            Some(&cookie),
            &[("folder_id", "")],
            &[("twenty.txt", "text/plain", b"12345678901234567890")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer.location.unwrap_or_default();
    assert!(
        location.contains("quota-exceeded"),
        "small upload was not refused: {location}"
    );
}

#[tokio::test]
async fn thumbnail_is_served_after_image_upload() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-thumb", "thumb@in.test", "Thumb").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-thumb")
        .await
        .unwrap()
        .unwrap();

    let png = tiny_png();
    let answer = app
        .post_multipart(
            "/files",
            Some(&cookie),
            &[("folder_id", "")],
            &[("dot.png", "image/png", &png)],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or("")
            .contains("refusal="),
        "image upload was refused: {}",
        answer.location.unwrap_or_default()
    );
    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.name == "dot.png")
        .expect("image was not stored");
    assert_eq!(file.mime, "image/png");

    let thumb = app.get(&format!("/thumb/{}", file.id), Some(&cookie)).await;
    assert_eq!(thumb.status, StatusCode::OK, "no thumbnail served");
    assert_eq!(thumb.content_type.as_deref(), Some("image/webp"));
    assert_eq!(&thumb.bytes[0..4], b"RIFF");
    assert_eq!(&thumb.bytes[8..12], b"WEBP");

    let etag = thumb.etag.clone().expect("no etag on the thumbnail");
    let cached = app
        .get_with_if_none_match(&format!("/thumb/{}", file.id), Some(&cookie), &etag)
        .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn cross_owner_answers_not_found() {
    let app = TestApp::build().await;
    let alice = app.sign_in("sub-alice", "alice@in.test", "Alice").await;
    let alice_user = app
        .store
        .user_by_oidc_sub("sub-alice")
        .await
        .unwrap()
        .unwrap();
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;

    app.post(
        "/api/folder/create",
        Some(&alice),
        &[("parent_id", ""), ("name", "hers")],
    )
    .await;
    let hers = folder_id(&app, &alice_user.id, None, "hers").await;
    app.post_multipart(
        "/files",
        Some(&alice),
        &[("folder_id", "")],
        &[("hers.txt", "text/plain", b"hers")],
    )
    .await;
    let file = app
        .store
        .list_children(&alice_user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap();

    let bytes = app.get(&format!("/file/{}", file.id), Some(&bob)).await;
    assert_eq!(bytes.status, StatusCode::NOT_FOUND);
    let thumb = app.get(&format!("/thumb/{}", file.id), Some(&bob)).await;
    assert_eq!(thumb.status, StatusCode::NOT_FOUND);
    let page = app.get(&format!("/drive?folder={hers}"), Some(&bob)).await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);

    let answer = app
        .post(
            "/api/folder/rename",
            Some(&bob),
            &[("id", &hers), ("name", "theirs")],
        )
        .await;
    assert!(
        answer.body.contains("NotFound"),
        "cross-owner write was not refused as not-found: {}",
        answer.body
    );
    let answer = app
        .post("/api/file/delete", Some(&bob), &[("id", &file.id)])
        .await;
    assert!(
        answer.body.contains("NotFound"),
        "cross-owner delete was not refused as not-found: {}",
        answer.body
    );
}

#[tokio::test]
async fn file_rename_move_and_delete() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-fops", "fops@in.test", "Fops").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-fops")
        .await
        .unwrap()
        .unwrap();

    app.post(
        "/api/folder/create",
        Some(&cookie),
        &[("parent_id", ""), ("name", "box")],
    )
    .await;
    let boxed = folder_id(&app, &user.id, None, "box").await;
    app.post_multipart(
        "/files",
        Some(&cookie),
        &[("folder_id", "")],
        &[("note.txt", "text/plain", b"note")],
    )
    .await;
    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap();

    let answer = app
        .post(
            "/api/file/rename",
            Some(&cookie),
            &[("id", &file.id), ("name", "renamed.txt")],
        )
        .await;
    assert_eq!(answer.body, "null", "renaming was refused: {}", answer.body);
    let answer = app
        .post(
            "/api/file/move",
            Some(&cookie),
            &[("id", &file.id), ("folder_id", &boxed)],
        )
        .await;
    assert_eq!(answer.body, "null", "moving was refused: {}", answer.body);
    let moved = app.store.file(&file.id).await.unwrap().unwrap();
    assert_eq!(moved.name, "renamed.txt");
    assert_eq!(moved.folder_id.as_deref(), Some(boxed.as_str()));

    let answer = app
        .post("/api/file/delete", Some(&cookie), &[("id", &file.id)])
        .await;
    assert_eq!(answer.body, "null", "deleting was refused: {}", answer.body);
    let trashed = app.store.file(&file.id).await.unwrap().unwrap();
    assert!(
        trashed.deleted_at.is_some(),
        "delete did not trash the file"
    );
    // And the bytes answer 404 once trashed.
    let gone = app.get(&format!("/file/{}", file.id), Some(&cookie)).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_abort_and_bad_chunk() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-abort", "abort@in.test", "Abort").await;

    let answer = app
        .post_json(
            "/api/upload/start",
            Some(&cookie),
            serde_json::json!({"folder_id": null, "name": "never.bin", "size_bytes": 100}),
        )
        .await;
    let session = answer.json()["ok"]["id"].as_str().unwrap().to_string();

    // A short piece is a bad chunk, not a short file.
    let answer = app
        .put_bytes(&format!("/api/upload/{session}/0"), Some(&cookie), b"short")
        .await;
    assert_eq!(
        answer.json()["err"].as_str(),
        Some("BadChunk"),
        "short chunk was not refused: {}",
        answer.body
    );

    let answer = app
        .post_json(
            &format!("/api/upload/{session}/abort"),
            Some(&cookie),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(
        answer.json()["ok"],
        serde_json::Value::Null,
        "{}",
        answer.body
    );

    // The finish after the abort finds an expired session, with the chunks
    // still staged for nothing.
    let answer = app
        .post_json(
            &format!("/api/upload/{session}/finish"),
            Some(&cookie),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(
        answer.json()["err"].as_str(),
        Some("UploadExpired"),
        "finish after abort was not refused: {}",
        answer.body
    );
}

#[tokio::test]
async fn forced_download_is_attachment() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-dl", "dl@in.test", "Dl").await;
    let user = app.store.user_by_oidc_sub("sub-dl").await.unwrap().unwrap();

    app.post_multipart(
        "/files",
        Some(&cookie),
        &[("folder_id", "")],
        &[("readme.txt", "text/plain", b"read me")],
    )
    .await;
    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap();

    let inline = app.get(&format!("/file/{}", file.id), Some(&cookie)).await;
    assert!(
        inline
            .disposition
            .as_deref()
            .unwrap_or("")
            .starts_with("inline"),
        "text should render inline: {:?}",
        inline.disposition
    );
    let forced = app
        .get(&format!("/file/{}?dl=1", file.id), Some(&cookie))
        .await;
    assert_eq!(forced.status, StatusCode::OK);
    assert!(
        forced
            .disposition
            .as_deref()
            .unwrap_or("")
            .starts_with("attachment"),
        "?dl=1 did not force attachment: {:?}",
        forced.disposition
    );
    assert_eq!(forced.bytes, b"read me");
}

/// Looking is not downloading: the viewer's inline serves — the plain GET
/// and a media element's range probe from byte 0 — never move the counter;
/// only the `?dl=1` disposition does, once per download.
#[tokio::test]
async fn previews_do_not_count_as_downloads() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-prev", "prev@in.test", "Prev").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-prev")
        .await
        .unwrap()
        .unwrap();

    app.post_multipart(
        "/files",
        Some(&cookie),
        &[("folder_id", "")],
        &[("clip.txt", "text/plain", b"counted only when taken")],
    )
    .await;
    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .next()
        .unwrap();
    let count = || async {
        app.store
            .file(&file.id)
            .await
            .unwrap()
            .unwrap()
            .download_count
    };

    let inline = app.get(&format!("/file/{}", file.id), Some(&cookie)).await;
    assert_eq!(inline.status, StatusCode::OK);
    let probe = app
        .get_with_range(&format!("/file/{}", file.id), Some(&cookie), "bytes=0-3")
        .await;
    assert_eq!(probe.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        count().await,
        0,
        "previews counted: inline and range serves"
    );

    let taken = app
        .get(&format!("/file/{}?dl=1", file.id), Some(&cookie))
        .await;
    assert_eq!(taken.status, StatusCode::OK);
    assert_eq!(count().await, 1, "a forced download did not count");
    // A mid-file chunk of a download is the same download going on.
    let resumed = app
        .get_with_range(
            &format!("/file/{}?dl=1", file.id),
            Some(&cookie),
            "bytes=4-",
        )
        .await;
    assert_eq!(resumed.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(count().await, 1, "a resumed chunk counted again");
}

// -- public links (merged from http_d2.rs; one binary per crate) ------------

#[tokio::test]
async fn link_create_public_view_revoke_gone() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "notes.txt", b"hello in").await;

    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());

    // Public: no cookie, the card renders.
    let page = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("notes.txt"), "{}", page.text());

    // Public download carries the bytes.
    let bytes = app.get(&format!("/s/{token}?dl=1"), None).await;
    assert_eq!(bytes.status, StatusCode::OK);
    assert_eq!(bytes.bytes, b"hello in");

    // Revoke: only the id travels; look it up straight off the store.
    let id = app.store.share_links(&owner).await.unwrap()[0].id.clone();
    let answer = app
        .post("/api/share/link/revoke", Some(&admin), &[("id", &id)])
        .await;
    assert!(answer.accepted(), "revoke refused: {:?}", answer.location);

    let gone = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(gone.status, StatusCode::OK);
    assert!(
        gone.text().contains("no longer works"),
        "revoked link showed: {}",
        gone.text()
    );
}

#[tokio::test]
async fn expired_link_is_dead() {
    let app = TestApp::build().await;
    let _ = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "old.txt", b"old").await;
    let past = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    let created = app
        .store
        .create_share_link(&owner, ShareKind::File, &file, true, Some(past))
        .await
        .unwrap();
    let page = app.get(&format!("/s/{}", created.token), None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.text().contains("no longer works"),
        "expired link showed: {}",
        page.text()
    );
}

#[tokio::test]
async fn view_only_link_download_is_dead_card() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "secret.txt", b"eyes only").await;
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "0"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());

    let page = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("secret.txt"), "{}", page.text());

    let blocked = app.get(&format!("/s/{token}?dl=1"), None).await;
    assert_eq!(blocked.status, StatusCode::OK, "{}", blocked.text());
    assert!(
        blocked.text().contains("no longer works"),
        "view-only download showed: {}",
        blocked.text()
    );
}

/// An unchecked `can_download` box posts nothing at all, and nothing posted
/// must never mint a download: the absent flag reads view-only, the way an
/// explicitly refused one does — while the checked box still opens the bytes.
#[tokio::test]
async fn link_create_without_the_flag_is_view_only() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "plain.txt", b"plain bytes").await;
    // No can_download pair — what an unchecked checkbox posts.
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[("kind", "file"), ("target_id", &file)],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());
    let page = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("plain.txt"), "{}", page.text());
    let blocked = app.get(&format!("/s/{token}?dl=1"), None).await;
    assert_eq!(blocked.status, StatusCode::OK, "{}", blocked.text());
    assert!(
        blocked.text().contains("no longer works"),
        "absent-flag download showed: {}",
        blocked.text()
    );
    // Both share surfaces print the target's name, never its raw id.
    let named = app
        .get(&format!("/drive?share=file:{file}"), Some(&admin))
        .await;
    assert_eq!(named.status, StatusCode::OK, "{}", named.text());
    assert!(
        named.text().contains("plain.txt") && named.text().contains("modal-scrim"),
        "share modal showed no name: {}",
        named.text()
    );
    let settings = app.get("/settings", Some(&admin)).await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.text());
    assert!(
        settings.text().contains("file · plain.txt"),
        "settings panel showed no name: {}",
        settings.text()
    );
    // The checked box still opens the bytes.
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());
    let bytes = app.get(&format!("/s/{token}?dl=1"), None).await;
    assert_eq!(bytes.status, StatusCode::OK);
    assert_eq!(bytes.bytes, b"plain bytes");
}

#[tokio::test]
async fn folder_link_lists_and_serves_children() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let folder = app
        .store
        .create_folder(&owner, None, "shared")
        .await
        .unwrap();
    app.store
        .insert_file(&owner, Some(&folder.id), "child.txt", b"child bytes")
        .await
        .unwrap();
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "folder"),
                ("target_id", &folder.id),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());

    let page = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("child.txt"), "{}", page.text());

    let child = app
        .store
        .list_children(&owner, Some(&folder.id))
        .await
        .unwrap()
        .files[0]
        .id
        .clone();
    let bytes = app
        .get(
            &format!("/s/{token}?folder={}&file={child}&dl=1", folder.id),
            None,
        )
        .await;
    assert_eq!(bytes.status, StatusCode::OK, "{}", bytes.text());
    assert_eq!(bytes.bytes, b"child bytes");
}

#[tokio::test]
async fn cross_owner_link_create_is_not_found() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let _ = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let bob = owner_of(&app, "sub-bob").await;
    let file = file_id(&app, &bob, "bobs.txt", b"bob").await;
    // Ada names Bob's file: answered as not-found, never forbidden.
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(
        answer.refused("not-found", "create"),
        "{:?}",
        answer.location
    );
}

// -- per-user shares --------------------------------------------------------

#[tokio::test]
async fn per_user_share_lands_in_shared_with_me() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let cara = app.sign_in("sub-cara", "cara@in.test", "Cara").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "joint.txt", b"joint").await;

    let answer = app
        .post(
            "/api/share/user/add",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "BOB@in.test"),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "add refused: {:?}", answer.location);

    let bobs = app.get("/shared", Some(&bob)).await;
    assert_eq!(bobs.status, StatusCode::OK, "{}", bobs.text());
    assert!(bobs.text().contains("joint.txt"), "{}", bobs.text());

    let caras = app.get("/shared", Some(&cara)).await;
    assert_eq!(caras.status, StatusCode::OK, "{}", caras.text());
    assert!(!caras.text().contains("joint.txt"), "{}", caras.text());

    // Unknown addresses are not-found, not a stack.
    let answer = app
        .post(
            "/api/share/user/add",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "ghost@in.test"),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.refused("not-found", "add"), "{:?}", answer.location);

    // Unshare and it leaves Bob's page.
    let answer = app
        .post(
            "/api/share/user/remove",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "bob@in.test"),
            ],
        )
        .await;
    assert!(answer.accepted(), "remove refused: {:?}", answer.location);
    let bobs = app.get("/shared", Some(&bob)).await;
    assert!(!bobs.text().contains("joint.txt"), "{}", bobs.text());
}

/// View only means preview, no download: the reader sees the media inline
/// on the viewer page and reads the inline bytes, while `?dl=1` stays dead
/// and the page never offers the Download link.
#[tokio::test]
async fn view_only_reader_previews_media_but_cannot_download() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(
        &app,
        &owner,
        "photo.png",
        b"\x89PNG\r\n\x1a\n preview bytes",
    )
    .await;

    let answer = app
        .post(
            "/api/share/user/add",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "bob@in.test"),
                ("can_download", "0"),
            ],
        )
        .await;
    assert!(answer.accepted(), "add refused: {:?}", answer.location);

    // The viewer page renders the media, not the unavailable note, and
    // offers no download.
    let page = app.get(&format!("/view/{file}"), Some(&bob)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("viewer-media"), "{}", page.text());
    assert!(!page.text().contains("No preview"), "{}", page.text());
    assert!(!page.text().contains("?dl=1"), "{}", page.text());

    // The inline bytes serve for the preview...
    let inline = app.get(&format!("/file/{file}"), Some(&bob)).await;
    assert_eq!(inline.status, StatusCode::OK, "{}", inline.text());
    assert_eq!(inline.bytes, b"\x89PNG\r\n\x1a\n preview bytes");
    // ...and taking the file away stays the stranger's answer.
    let taken = app.get(&format!("/file/{file}?dl=1"), Some(&bob)).await;
    assert_eq!(taken.status, StatusCode::NOT_FOUND, "{}", taken.text());

    // A preview is not a download: the counter never moves.
    assert_eq!(
        app.store.file(&file).await.unwrap().unwrap().download_count,
        0
    );
}

// -- trash ------------------------------------------------------------------

#[tokio::test]
async fn trash_restore_purge_empty_flows() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let one = file_id(&app, &owner, "one.txt", b"one").await;
    let two = file_id(&app, &owner, "two.txt", b"two").await;
    app.store.delete_file(&one).await.unwrap();
    app.store.delete_file(&two).await.unwrap();

    let page = app.get("/trash", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("one.txt"), "{}", page.text());
    assert!(page.text().contains("two.txt"), "{}", page.text());

    // Restore one: it leaves the trash and keeps its bytes.
    let answer = app
        .post(
            "/api/trash/restore",
            Some(&admin),
            &[("kind", "file"), ("id", &one)],
        )
        .await;
    assert!(answer.accepted(), "restore refused: {:?}", answer.location);
    assert!(
        app.store
            .file(&one)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );

    // Purge the other: the row and its bytes go.
    let answer = app
        .post(
            "/api/trash/purge",
            Some(&admin),
            &[("kind", "file"), ("id", &two)],
        )
        .await;
    assert!(answer.accepted(), "purge refused: {:?}", answer.location);
    assert!(app.store.file(&two).await.unwrap().is_none());

    // Empty takes the rest.
    let three = file_id(&app, &owner, "three.txt", b"three").await;
    app.store.delete_file(&three).await.unwrap();
    let answer = app.post("/api/trash/empty", Some(&admin), &[]).await;
    assert!(answer.accepted(), "empty refused: {:?}", answer.location);
    assert!(app.store.list_trash(&owner).await.unwrap().files.is_empty());
}

#[tokio::test]
async fn restore_under_trashed_ancestor_is_refused() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let folder = app.store.create_folder(&owner, None, "box").await.unwrap();
    let file = app
        .store
        .insert_file(&owner, Some(&folder.id), "inbox.txt", b"in")
        .await
        .unwrap();
    app.store.delete_folder(&folder.id).await.unwrap();

    let answer = app
        .post(
            "/api/trash/restore",
            Some(&admin),
            &[("kind", "file"), ("id", &file.id)],
        )
        .await;
    assert!(
        answer.refused("ancestor-trashed", "restore"),
        "{:?}",
        answer.location
    );

    // The folder itself restores with its child.
    let answer = app
        .post(
            "/api/trash/restore",
            Some(&admin),
            &[("kind", "folder"), ("id", &folder.id)],
        )
        .await;
    assert!(answer.accepted(), "restore refused: {:?}", answer.location);
}

#[tokio::test]
async fn folder_purge_destroys_its_tree() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let folder = app
        .store
        .create_folder(&owner, None, "doomed")
        .await
        .unwrap();
    let file = app
        .store
        .insert_file(&owner, Some(&folder.id), "gone.txt", b"gone")
        .await
        .unwrap();
    app.store.delete_folder(&folder.id).await.unwrap();

    let answer = app
        .post(
            "/api/trash/purge",
            Some(&admin),
            &[("kind", "folder"), ("id", &folder.id)],
        )
        .await;
    assert!(answer.accepted(), "purge refused: {:?}", answer.location);
    assert!(app.store.folder(&folder.id).await.unwrap().is_none());
    assert!(app.store.file(&file.id).await.unwrap().is_none());
}

// -- search -----------------------------------------------------------------

#[tokio::test]
async fn search_scopes_to_owner_and_hides_trashed() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let ada = owner_of(&app, "sub-admin").await;
    let bob_id = owner_of(&app, "sub-bob").await;
    let mine = file_id(&app, &ada, "quarterly report.txt", b"a").await;
    let _ = file_id(&app, &bob_id, "quarterly notes.txt", b"b").await;
    let buried = file_id(&app, &ada, "quarterly draft.txt", b"c").await;
    app.store.delete_file(&buried).await.unwrap();

    // Empty query is the folder view, not the whole library under a new
    // name: the upload control renders, no results caption does.
    let page = app.get("/drive", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("id=\"upload-form\""),
        "{}",
        page.text()
    );
    assert!(!page.text().contains("Search results"), "{}", page.text());

    let page = app.get("/drive?q=quarterly", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("quarterly report.txt"),
        "{}",
        page.text()
    );
    assert!(
        !page.text().contains("quarterly notes.txt"),
        "{}",
        page.text()
    );
    assert!(
        !page.text().contains("quarterly draft.txt"),
        "{}",
        page.text()
    );
    assert!(app.store.file(&mine).await.unwrap().is_some());

    let page = app.get("/drive?q=quarterly", Some(&bob)).await;
    assert!(
        page.text().contains("quarterly notes.txt"),
        "{}",
        page.text()
    );
    assert!(
        !page.text().contains("quarterly report.txt"),
        "{}",
        page.text()
    );
}

#[tokio::test]
async fn drive_search_results_merge_into_one_panel() {
    let app = TestApp::build().await;
    let cookie = app
        .sign_in("sub-search-in-place", "inplace@in.test", "InPlace")
        .await;
    let owner = owner_of(&app, "sub-search-in-place").await;
    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "holiday photos")],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let folder = folder_id(&app, &owner, None, "holiday photos").await;
    let file = file_id(&app, &owner, "holiday report.txt", b"sun").await;

    let page = app.get("/drive?q=holiday", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    // Both hits render in one shared list, folders first, wired to their surfaces.
    assert!(body.contains("holiday photos"), "{body}");
    assert!(body.contains("holiday report.txt"), "{body}");
    assert!(body.contains(&format!("/drive?folder={folder}")), "{body}");
    assert!(body.contains(&format!("/view/{file}")), "{body}");
    let folder_at = body.find("holiday photos").expect("no folder hit");
    let file_at = body.find("holiday report.txt").expect("no file hit");
    assert!(folder_at < file_at, "file hit precedes folder hit: {body}");
    // One panel, no heads or chips.
    assert_eq!(
        body.matches("<section class=\"panel drive-panel\">")
            .count(),
        1,
        "{body}"
    );
    assert!(!body.contains("panel-head"), "{body}");
    assert!(!body.contains("class=\"chip\""), "{body}");
    assert!(!body.contains(">Folders</h2>"), "{body}");
    assert!(!body.contains(">Files</h2>"), "{body}");
    // The caption names the query and the box keeps it.
    assert!(body.contains("Search results"), "{body}");
    assert!(body.contains("holiday"), "{body}");
    assert!(body.contains("value=\"holiday\""), "{body}");
    assert!(body.contains("name=\"q\""), "{body}");
    // The upload control stays in the DOM for the + menu and the drop handler.
    assert!(body.contains("id=\"upload-form\""), "{body}");
    assert!(body.contains("id=\"drive-upload-input\""), "{body}");
    // The folder view is gone: no crumbs, no row mutations.
    assert!(!body.contains("detail-crumbs"), "{body}");
    assert!(!body.contains("action=\"/api/folder/rename\""), "{body}");
}

#[tokio::test]
async fn drive_search_never_shows_other_owners_hits() {
    let app = TestApp::build().await;
    let ada = app.sign_in("sub-search-ada", "ada2@in.test", "Ada").await;
    let bob = app.sign_in("sub-search-bob", "bob2@in.test", "Bob").await;
    let bob_id = owner_of(&app, "sub-search-bob").await;
    let _ = file_id(&app, &bob_id, "secret ledger.txt", b"b").await;
    let answer = app
        .post(
            "/api/folder/create",
            Some(&bob),
            &[("parent_id", ""), ("name", "secret drawer")],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);

    // Ada's library-wide search stays hers: Bob's file and folder hits never
    // appear, and the quiet note says so.
    let page = app.get("/drive?q=secret", Some(&ada)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    assert!(!body.contains("secret ledger.txt"), "{body}");
    assert!(!body.contains("secret drawer"), "{body}");
    assert!(body.contains("Nothing found."), "{body}");

    // Sanity: the same query finds them for their owner.
    let page = app.get("/drive?q=secret", Some(&bob)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("secret ledger.txt"), "{}", page.text());
    assert!(page.text().contains("secret drawer"), "{}", page.text());
}

#[tokio::test]
async fn drive_search_empty_query_is_the_folder_view() {
    let app = TestApp::build().await;
    let cookie = app
        .sign_in("sub-search-empty", "empty@in.test", "Empty")
        .await;
    let owner = owner_of(&app, "sub-search-empty").await;
    let _ = file_id(&app, &owner, "plain.txt", b"p").await;

    for path in ["/drive?q=", "/drive?q=%20%20"] {
        let page = app.get(path, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK, "{}: {}", path, page.text());
        let body = page.text();
        // The folder listing renders as usual — the file is there through
        // its directory, not through a result — with the box held empty.
        assert!(body.contains("plain.txt"), "{path}: {body}");
        assert!(body.contains("id=\"upload-form\""), "{path}: {body}");
        assert!(body.contains("name=\"q\""), "{path}: {body}");
        assert!(!body.contains("Search results"), "{path}: {body}");
    }
}

#[tokio::test]
async fn old_search_address_redirects_to_drive() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-search-old", "old@in.test", "Old").await;

    let page = app.get("/search?q=quarterly", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.text());
    assert_eq!(page.location.as_deref(), Some("/drive?q=quarterly"));

    // The raw pair rides through untouched, so encoded queries survive.
    let page = app.get("/search?q=holiday+report", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.text());
    assert_eq!(page.location.as_deref(), Some("/drive?q=holiday+report"));

    // No query is the plain drive; no session still redirects, never 401s.
    let page = app.get("/search", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.text());
    assert_eq!(page.location.as_deref(), Some("/drive"));
    let page = app.get("/search?q=quarterly", None).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.text());
    assert_eq!(page.location.as_deref(), Some("/drive?q=quarterly"));
}

#[tokio::test]
async fn topbar_nav_has_no_search_link() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-search-nav", "nav@in.test", "Nav").await;
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    let nav_start = body.find("topbar-nav-links").expect("no nav");
    let nav_end = body[nav_start..].find("</nav>").expect("nav never closes");
    let nav = &body[nav_start..nav_start + nav_end];
    // Drive, Shared, Trash — and nothing else.
    assert!(nav.contains("href=\"/drive\""), "{nav}");
    assert!(nav.contains("href=\"/shared\""), "{nav}");
    assert!(nav.contains("href=\"/trash\""), "{nav}");
    assert!(
        !nav.contains("/search"),
        "search leaked into the nav: {nav}"
    );
    assert!(!nav.contains("Search"), "search leaked into the nav: {nav}");
}

// -- settings ---------------------------------------------------------------

#[tokio::test]
async fn settings_quota_and_disable_guards() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let bob_id = owner_of(&app, "sub-bob").await;
    let admin_id = owner_of(&app, "sub-admin").await;

    let page = app.get("/settings", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("ada@in.test"), "{}", page.text());

    // Admin sets Bob's quota in human units: 2 GiB lands as 2147483648 bytes.
    let answer = app
        .post(
            "/api/settings/quota",
            Some(&admin),
            &[("user_id", &bob_id), ("quota", "2"), ("quota_unit", "GiB")],
        )
        .await;
    assert!(answer.accepted(), "quota refused: {:?}", answer.location);
    assert_eq!(
        app.store.user(&bob_id).await.unwrap().unwrap().quota_bytes,
        2147483648
    );

    // A non-admin is refused the same call.
    let answer = app
        .post(
            "/api/settings/quota",
            Some(&bob),
            &[
                ("user_id", &bob_id),
                ("quota", "999"),
                ("quota_unit", "MiB"),
            ],
        )
        .await;
    assert!(
        answer.refused("forbidden", "quota"),
        "{:?}",
        answer.location
    );

    // Disabling yourself is refused; disabling Bob signs him out.
    let answer = app
        .post(
            "/api/settings/disable",
            Some(&admin),
            &[("user_id", &admin_id), ("disabled", "1")],
        )
        .await;
    assert!(
        answer.refused("forbidden", "disable"),
        "{:?}",
        answer.location
    );

    let answer = app
        .post(
            "/api/settings/disable",
            Some(&admin),
            &[("user_id", &bob_id), ("disabled", "1")],
        )
        .await;
    assert!(answer.accepted(), "disable refused: {:?}", answer.location);
    let page = app.get("/settings", Some(&bob)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        !page.text().contains("bob@in.test"),
        "disabled account still served: {}",
        page.text()
    );
}

#[tokio::test]
async fn signed_out_mutations_ask_to_sign_in() {
    let app = TestApp::build().await;
    for (path, call, form) in [
        (
            "/api/share/link/create",
            "create",
            &[("kind", "file"), ("target_id", "x"), ("can_download", "1")][..],
        ),
        ("/api/share/link/revoke", "revoke", &[("id", "x")][..]),
        (
            "/api/share/user/add",
            "add",
            &[("kind", "file"), ("target_id", "x"), ("email", "a@b.c")][..],
        ),
        (
            "/api/trash/restore",
            "restore",
            &[("kind", "file"), ("id", "x")][..],
        ),
        ("/api/trash/empty", "empty", &[][..]),
        (
            "/api/settings/quota",
            "quota",
            &[("user_id", "x"), ("quota", "1"), ("quota_unit", "GiB")][..],
        ),
    ] {
        let answer = app.post(path, None, form).await;
        assert!(
            answer.refused("sign-in-first", call),
            "{path}: {:?}",
            answer.location
        );
    }
    // A never-real token is the dead card, not a stack.
    let page = app.get("/s/never-real-token", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("no longer works"), "{}", page.text());
}

#[tokio::test]
async fn empty_file_upload_round_trip() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-empty", "empty@in.test", "Empty").await;
    let user = app
        .store
        .user_by_oidc_sub("sub-empty")
        .await
        .unwrap()
        .unwrap();

    let answer = app
        .post_multipart(
            "/files",
            Some(&cookie),
            &[("folder_id", "")],
            &[("empty.txt", "text/plain", &[])],
        )
        .await;
    assert_eq!(
        answer.status,
        StatusCode::SEE_OTHER,
        "{}",
        answer.location.clone().unwrap_or_default()
    );
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or("")
            .contains("refusal="),
        "empty upload was refused: {}",
        answer.location.clone().unwrap_or_default()
    );

    let file = app
        .store
        .list_children(&user.id, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.name == "empty.txt")
        .expect("empty file was not stored");
    assert_eq!(file.size_bytes, 0);

    let got = app.get(&format!("/file/{}", file.id), Some(&cookie)).await;
    assert_eq!(got.status, StatusCode::OK);
    assert!(
        got.bytes.is_empty(),
        "expected 0 bytes, got {}",
        got.bytes.len()
    );
}

// -- share review regressions (FullSilkworm) ------------------------------------

#[tokio::test]
async fn view_only_download_matches_dead_token() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "quiet.txt", b"quiet").await;
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "0"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());

    // A view-only download attempt is the dead card — the same answer as a
    // token that never existed, so the surface never says which tokens exist.
    let blocked = app.get(&format!("/s/{token}?dl=1"), None).await;
    let dead = app.get("/s/never-real-token", None).await;
    assert_eq!(blocked.status, dead.status, "{}", blocked.text());
    assert_eq!(blocked.status, StatusCode::OK, "{}", blocked.text());
    assert!(
        blocked.text().contains("no longer works"),
        "view-only download showed: {}",
        blocked.text()
    );
    assert!(
        dead.text().contains("no longer works"),
        "dead token showed: {}",
        dead.text()
    );
}

#[tokio::test]
async fn malformed_expiry_is_forbidden() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "lease.txt", b"lease").await;
    // Non-numeric, zero and negative values must not mint a link — and must
    // never silently mint a never-expiring one.
    for raw in ["not-a-number", "0", "-2", "1.5"] {
        let answer = app
            .post(
                "/api/share/link/create",
                Some(&admin),
                &[
                    ("kind", "file"),
                    ("target_id", &file),
                    ("can_download", "1"),
                    ("expires_in_days", raw),
                ],
            )
            .await;
        assert!(
            answer.refused("forbidden", "create"),
            "{raw:?}: {:?}",
            answer.location
        );
    }
    assert!(
        app.store.share_links(&owner).await.unwrap().is_empty(),
        "a refused mint left a link behind"
    );
    // A positive value still mints an expiring link; an empty one still means
    // no expiry.
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "1"),
                ("expires_in_days", "30"),
            ],
        )
        .await;
    assert!(
        answer.accepted(),
        "good expiry refused: {:?}",
        answer.location
    );
    let links = app.store.share_links(&owner).await.unwrap();
    assert_eq!(links.len(), 1);
    assert!(
        links[0].expires_at.is_some(),
        "a 30-day link carries no expiry"
    );
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("can_download", "1"),
                ("expires_in_days", ""),
            ],
        )
        .await;
    assert!(
        answer.accepted(),
        "empty expiry refused: {:?}",
        answer.location
    );
    let links = app.store.share_links(&owner).await.unwrap();
    assert_eq!(links.len(), 2);
    assert!(
        links.iter().any(|link| link.expires_at.is_none()),
        "an empty expiry should mean no expiry"
    );
}

#[tokio::test]
async fn unshare_trashed_target_still_revokes() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let bob = app.sign_in("sub-bob", "bob@in.test", "Bob").await;
    let owner = owner_of(&app, "sub-admin").await;
    let file = file_id(&app, &owner, "doomed.txt", b"doomed").await;
    let answer = app
        .post(
            "/api/share/user/add",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "bob@in.test"),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "add refused: {:?}", answer.location);
    let bobs = app.get("/shared", Some(&bob)).await;
    assert!(bobs.text().contains("doomed.txt"), "{}", bobs.text());

    // Trash the target: unsharing must still go through for the owner.
    app.store.delete_file(&file).await.unwrap();
    let answer = app
        .post(
            "/api/share/user/remove",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "bob@in.test"),
            ],
        )
        .await;
    assert!(answer.accepted(), "remove refused: {:?}", answer.location);
    // The grant row is really gone: restoring the file does not bring the
    // share back to Bob's page.
    app.store.restore_file(&file).await.unwrap();
    let bobs = app.get("/shared", Some(&bob)).await;
    assert!(!bobs.text().contains("doomed.txt"), "{}", bobs.text());
}

#[tokio::test]
async fn view_only_public_thumb_serves_webp() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-admin", "ada@in.test", "Ada").await;
    let owner = owner_of(&app, "sub-admin").await;
    let png = tiny_png();
    let answer = app
        .post_multipart(
            "/files",
            Some(&admin),
            &[("folder_id", "")],
            &[("dot.png", "image/png", &png)],
        )
        .await;
    assert!(
        !answer
            .location
            .as_deref()
            .unwrap_or("")
            .contains("refusal="),
        "image upload was refused: {}",
        answer.location.unwrap_or_default()
    );
    let file = app
        .store
        .list_children(&owner, None)
        .await
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.name == "dot.png")
        .expect("image was not stored");
    let answer = app
        .post(
            "/api/share/link/create",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file.id),
                ("can_download", "0"),
            ],
        )
        .await;
    assert!(answer.accepted(), "create refused: {:?}", answer.location);
    let token = created_token(answer.location.as_deref().unwrap());
    // The card previews through the thumb route, never the raw bytes.
    let page = app.get(&format!("/s/{token}"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("?thumb=1"),
        "card previews off nothing public: {}",
        page.text()
    );
    assert!(
        !page.text().contains("?dl=1"),
        "card must not point at the raw bytes: {}",
        page.text()
    );

    // View-only still grants the 512px preview: webp bytes, etag revalidates.
    let thumb = app.get(&format!("/s/{token}?thumb=1"), None).await;
    assert_eq!(thumb.status, StatusCode::OK, "{}", thumb.text());
    assert_eq!(thumb.content_type.as_deref(), Some("image/webp"));
    assert_eq!(&thumb.bytes[0..4], b"RIFF");
    assert_eq!(&thumb.bytes[8..12], b"WEBP");
    let etag = thumb.etag.clone().expect("no etag on the public thumbnail");
    let cached = app
        .get_with_if_none_match(&format!("/s/{token}?thumb=1"), None, &etag)
        .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);

    // The full bytes stay behind the download gate even so.
    let blocked = app.get(&format!("/s/{token}?dl=1"), None).await;
    assert_eq!(blocked.status, StatusCode::OK, "{}", blocked.text());
    assert!(
        blocked.text().contains("no longer works"),
        "view-only download showed: {}",
        blocked.text()
    );

    // A dead token's thumb is the dead card, not a stack.
    let dead = app.get("/s/never-real-token?thumb=1", None).await;
    assert_eq!(dead.status, StatusCode::OK, "{}", dead.text());
    assert!(
        dead.text().contains("no longer works"),
        "dead thumb showed: {}",
        dead.text()
    );
}

#[tokio::test]
async fn a_name_collision_upload_lands_postfixed() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-dupe", "dupe@in.test", "Dupe").await;

    // Three uploads under one name: none is refused — the later ones land
    // with a postfix before the extension instead of wasting the upload.
    for body in [b"first" as &[u8], b"second", b"third"] {
        let answer = app
            .post_multipart(
                "/files",
                Some(&cookie),
                &[("folder_id", "")],
                &[("same.txt", "text/plain", body)],
            )
            .await;
        assert!(answer.accepted(), "upload refused: {:?}", answer.location);
    }

    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("same.txt"), "{}", page.text());
    assert!(page.text().contains("same (2).txt"), "{}", page.text());
    assert!(page.text().contains("same (3).txt"), "{}", page.text());
}

// -- settings preferences ------------------------------------------------------

#[tokio::test]
async fn settings_preferences_round_trip() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-prefs", "prefs@in.test", "Prefs").await;
    let me = owner_of(&app, "sub-prefs").await;

    // Instrument, dark and English are the defaults, worn on `<html>` from
    // the first render.
    let user = app.store.user(&me).await.unwrap().unwrap();
    assert_eq!(user.ui, "instrument");
    assert_eq!(user.theme, "dark");
    assert_eq!(user.language, "en");
    let page = app.get("/settings", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("data-ui=\"instrument\""),
        "{}",
        page.text()
    );
    assert!(
        page.text().contains("data-theme=\"dark\""),
        "{}",
        page.text()
    );
    assert!(page.text().contains("lang=\"en\""), "{}", page.text());

    // Saving all three writes the row and re-renders the chrome — away from
    // the defaults, so the render proves the write.
    let answer = app
        .post(
            "/api/settings/preferences",
            Some(&cookie),
            &[("ui", "ledger"), ("theme", "light"), ("language", "tr")],
        )
        .await;
    assert!(
        answer.accepted(),
        "preferences refused: {:?}",
        answer.location
    );
    let user = app.store.user(&me).await.unwrap().unwrap();
    assert_eq!(user.ui, "ledger");
    assert_eq!(user.theme, "light");
    assert_eq!(user.language, "tr");

    // The redirect carries the saved chip, rendered in the new language.
    let page = app
        .get(answer.location.as_deref().unwrap(), Some(&cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("Kaydedildi."), "{}", page.text());

    let page = app.get("/settings", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("data-ui=\"ledger\""),
        "{}",
        page.text()
    );
    // Light means no `data-theme` at all.
    assert!(!page.text().contains("data-theme="), "{}", page.text());
    assert!(page.text().contains("lang=\"tr\""), "{}", page.text());
    for name in ["name=\"ui\"", "name=\"theme\"", "name=\"language\""] {
        assert!(page.text().contains(name), "{}", page.text());
    }

    // And back again, to the defaults.
    let answer = app
        .post(
            "/api/settings/preferences",
            Some(&cookie),
            &[("ui", "instrument"), ("theme", "dark"), ("language", "en")],
        )
        .await;
    assert!(
        answer.accepted(),
        "preferences refused: {:?}",
        answer.location
    );
    let user = app.store.user(&me).await.unwrap().unwrap();
    assert_eq!(user.ui, "instrument");
    assert_eq!(user.theme, "dark");
    assert_eq!(user.language, "en");
}

#[tokio::test]
async fn settings_preferences_rejects_unknown_values() {
    let app = TestApp::build().await;
    let cookie = app
        .sign_in("sub-prefs-bad", "prefsbad@in.test", "PrefsBad")
        .await;
    let me = owner_of(&app, "sub-prefs-bad").await;

    // Each field is refused with its own code, and the row keeps the last
    // good write.
    let answer = app
        .post(
            "/api/settings/preferences",
            Some(&cookie),
            &[("ui", "mosaic"), ("theme", "light"), ("language", "en")],
        )
        .await;
    assert!(
        answer.refused("bad-ui", "preferences"),
        "{:?}",
        answer.location
    );
    let answer = app
        .post(
            "/api/settings/preferences",
            Some(&cookie),
            &[("ui", "ledger"), ("theme", "dim"), ("language", "en")],
        )
        .await;
    assert!(
        answer.refused("bad-theme", "preferences"),
        "{:?}",
        answer.location
    );
    let answer = app
        .post(
            "/api/settings/preferences",
            Some(&cookie),
            &[("ui", "ledger"), ("theme", "light"), ("language", "xx")],
        )
        .await;
    assert!(
        answer.refused("bad-language", "preferences"),
        "{:?}",
        answer.location
    );
    let user = app.store.user(&me).await.unwrap().unwrap();
    assert_eq!(user.ui, "instrument");
    assert_eq!(user.theme, "dark");
    assert_eq!(user.language, "en");
    let page = app
        .get(answer.location.as_deref().unwrap(), Some(&cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(
        page.text().contains("That is not a language."),
        "{}",
        page.text()
    );

    // Signed out, the same post asks to sign in first.
    let answer = app
        .post(
            "/api/settings/preferences",
            None,
            &[("ui", "instrument"), ("theme", "dark"), ("language", "tr")],
        )
        .await;
    assert!(
        answer.refused("sign-in-first", "preferences"),
        "{:?}",
        answer.location
    );
}

impl TestApp {
    /// Like `get`, but carrying an `Accept-Language` header, for the
    /// signed-out language fallback.
    async fn get_with_lang(&self, path: &str, cookie: Option<&str>, lang: &str) -> Raw {
        let mut request = Request::builder()
            .method("GET")
            .uri(path)
            .header(header::ACCEPT_LANGUAGE, lang);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        let response = self
            .router
            .handle(request.body(Body::empty()).unwrap())
            .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Raw {
            status,
            content_type: None,
            disposition: None,
            content_range: None,
            accept_ranges: None,
            cache_control: None,
            etag: None,
            location: None,
            bytes,
        }
    }
}

#[tokio::test]
async fn signed_out_settings_falls_back_to_accept_language() {
    let app = TestApp::build().await;

    // No header: English chrome on a dark instrument shell.
    let page = app.get("/settings", None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("lang=\"en\""), "{}", page.text());
    assert!(
        page.text().contains("data-theme=\"dark\""),
        "{}",
        page.text()
    );
    assert!(
        page.text().contains("data-ui=\"instrument\""),
        "{}",
        page.text()
    );
    assert!(page.text().contains("Sign in first."), "{}", page.text());
    // A Turkish browser is answered in Turkish.
    let page = app
        .get_with_lang("/settings", None, "tr-TR,tr;q=0.9,en;q=0.8")
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("lang=\"tr\""), "{}", page.text());
    assert!(page.text().contains("Önce oturum aç."), "{}", page.text());
    assert!(page.text().contains("Sürücüye dön"), "{}", page.text());
}

#[tokio::test]
async fn settings_panels_nest_inside_the_stage() {
    let app = TestApp::build().await;
    let admin = app.sign_in("sub-stage", "stage@in.test", "Stage").await;
    let page = app.get("/settings", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    let stage = body
        .find("<main class=\"settings-stage")
        .expect("no settings-stage");
    let close = body.find("</main>").expect("no main close");
    for marker in ["panel-head", "member-table", "name=\"ui\""] {
        let at = body.find(marker).unwrap_or_else(|| panic!("no {marker}"));
        assert!(stage < at && at < close, "{marker} escapes the stage");
    }
    // The admin register is a table now: one named column per fact.
    assert!(body.contains("<th class=\"member-col-name\""), "{body}");
}

// The /shared and /trash strip fix: content belongs in the stage,
// never directly in the flex-row shell. Mirrors
// settings_panels_nest_inside_the_stage.
fn assert_stage_nests(body: &str, markers: &[&str]) {
    assert!(
        !body.contains("<main class=\"settings-shell\">"),
        "content still sits in the shell: {body}"
    );
    let stage = body
        .find("<main class=\"settings-stage")
        .expect("no settings-stage");
    let close = body.find("</main>").expect("no main close");
    assert!(stage < close, "unclosed stage");
    for marker in markers {
        let at = body.find(marker).unwrap_or_else(|| panic!("no {marker}"));
        assert!(stage < at && at < close, "{marker} escapes the stage");
    }
}

#[tokio::test]
async fn shared_lists_nest_inside_the_stage() {
    let app = TestApp::build().await;
    let admin = app
        .sign_in("sub-stage-sharer", "sharer@in.test", "Sharer")
        .await;
    let bob = app
        .sign_in("sub-stage-bob", "stagebob@in.test", "StageBob")
        .await;
    let owner = owner_of(&app, "sub-stage-sharer").await;
    let file = file_id(&app, &owner, "staged.txt", b"staged").await;
    let answer = app
        .post(
            "/api/share/user/add",
            Some(&admin),
            &[
                ("kind", "file"),
                ("target_id", &file),
                ("email", "stagebob@in.test"),
                ("can_download", "1"),
            ],
        )
        .await;
    assert!(answer.accepted(), "add refused: {:?}", answer.location);
    let page = app.get("/shared", Some(&bob)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert_stage_nests(
        &page.text(),
        &["panel-head", "panel-title", "staged.txt", "class=\"chip\""],
    );
}

#[tokio::test]
async fn trash_panels_nest_inside_the_stage() {
    let app = TestApp::build().await;
    let admin = app
        .sign_in("sub-stage-trash", "stagetrash@in.test", "StageTrash")
        .await;
    let owner = owner_of(&app, "sub-stage-trash").await;
    let one = file_id(&app, &owner, "staged-trash.txt", b"staged").await;
    app.store.delete_file(&one).await.unwrap();
    let page = app.get("/trash", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert_stage_nests(
        &page.text(),
        &[
            "panel-head",
            "staged-trash.txt",
            "action=\"/api/trash/restore\"",
            "action=\"/api/trash/empty\"",
        ],
    );
}

#[tokio::test]
async fn drive_search_panel_nests_inside_the_stage() {
    let app = TestApp::build().await;
    let admin = app
        .sign_in("sub-stage-search", "stagesearch@in.test", "StageSearch")
        .await;
    let owner = owner_of(&app, "sub-stage-search").await;
    file_id(&app, &owner, "staged-report.txt", b"staged").await;
    let page = app.get("/drive?q=staged-report", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert_stage_nests(
        &page.text(),
        &["drive-panel", "name=\"q\"", "staged-report.txt"],
    );
}

#[tokio::test]
async fn public_link_dead_card_survives_the_reskin() {
    let app = TestApp::build().await;
    // A never-real token is the dead card, not a stack.
    let page = app.get("/s/never-real-token", None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("no longer works"), "{}", page.text());
    assert!(
        page.text().contains("class=\"scaffold-note\""),
        "{}",
        page.text()
    );
}

#[tokio::test]
async fn drive_root_hides_crumbs_while_a_subfolder_shows_them() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-crumbs", "crumbs@in.test", "Crumbs").await;
    let owner = owner_of(&app, "sub-crumbs").await;

    // At the root the crumbs nav is pointless chrome: no self-link.
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(!page.text().contains("detail-crumbs"), "{}", page.text());
    // The filterbar itself stays, holding the search box and the create form.
    assert!(
        page.text().contains("class=\"filterbar drive-bar\""),
        "{}",
        page.text()
    );
    assert!(page.text().contains("name=\"q\""), "{}", page.text());
    assert!(
        page.text().contains("action=\"/api/folder/create\""),
        "{}",
        page.text()
    );

    // Inside a folder the way back renders.
    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "inside")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let folder = folder_id(&app, &owner, None, "inside").await;
    let page = app
        .get(&format!("/drive?folder={folder}"), Some(&cookie))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("detail-crumbs"), "{}", page.text());
    assert!(page.text().contains("inside"), "{}", page.text());
}

#[tokio::test]
async fn settings_quota_button_wears_the_quiet_class() {
    let app = TestApp::build().await;
    let admin = app
        .sign_in("sub-quota-btn", "quotabtn@in.test", "QuotaBtn")
        .await;
    let page = app.get("/settings", Some(&admin)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    // The Set-the-quota submit is a house button in a field row, not a
    // native gray one.
    assert!(
        body.contains("<form class=\"pop-row-form member-quota\""),
        "{body}"
    );
    let quota_at = body
        .find("action=\"/api/settings/quota\"")
        .expect("no quota form");
    let button_at = body[quota_at..]
        .find("<button class=\"quiet\" type=\"submit\">")
        .expect("quota submit is not quiet");
    let form_end = body[quota_at..]
        .find("</form>")
        .expect("quota form never closes");
    assert!(
        button_at < form_end,
        "quiet button escapes the quota form: {body}"
    );
}

#[tokio::test]
async fn settings_lives_in_the_user_menu_not_the_topbar() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-menu", "menu@in.test", "Menu").await;
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();

    // The page nav carries no settings link…
    let nav_start = body.find("topbar-nav-links").expect("no nav");
    let nav_end = body[nav_start..].find("</nav>").expect("nav never closes");
    let nav = &body[nav_start..nav_start + nav_end];
    assert!(
        !nav.contains("/settings"),
        "settings leaked into the nav: {nav}"
    );

    // …the user menu carries it, plus the profile link out to im.
    let menu_start = body.find("user-menu-panel").expect("no user menu");
    let menu = &body[menu_start..];
    assert!(menu.contains("href=\"/settings\""), "{menu}");
    let issuer = format!("href=\"{}/\"", app.config.oidc.issuer);
    assert!(
        menu.contains(&issuer),
        "no profile link to the issuer: {menu}"
    );
}

#[tokio::test]
async fn avatar_serves_own_photo_with_etag() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-face", "face@in.test", "Face").await;
    let me = owner_of(&app, "sub-face").await;
    app.fake.set_photo("sub-face", tiny_png(), "image/png");

    let face = app.get(&format!("/avatar/{me}"), Some(&cookie)).await;
    assert_eq!(face.status, StatusCode::OK);
    assert_eq!(face.content_type.as_deref(), Some("image/png"));
    assert_eq!(
        face.cache_control.as_deref(),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(face.bytes, tiny_png());

    let etag = face.etag.clone().expect("no etag on the avatar");
    let cached = app
        .get_with_if_none_match(&format!("/avatar/{me}"), Some(&cookie), &etag)
        .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    assert!(cached.bytes.is_empty());

    // The user menu wears the photo over the initials, with the fallback script.
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let body = page.text();
    assert!(
        body.contains(&format!("src=\"/avatar/{me}\"")),
        "menu wears no photo img"
    );
    assert!(
        body.contains("avatar-stack"),
        "no initials stack under the photo"
    );
    assert!(body.contains("__inAvatar"), "no avatar fallback script");
}

#[tokio::test]
async fn avatar_without_photo_is_not_found() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-noface", "noface@in.test", "NoFace").await;
    let me = owner_of(&app, "sub-noface").await;

    let face = app.get(&format!("/avatar/{me}"), Some(&cookie)).await;
    assert_eq!(face.status, StatusCode::NOT_FOUND);
    assert!(face.bytes.is_empty());
}

#[tokio::test]
async fn avatar_for_someone_else_is_not_found() {
    let app = TestApp::build().await;
    let alice = app
        .sign_in("sub-alice-face", "alice@in.test", "Alice")
        .await;
    let bob = app.sign_in("sub-bob-face", "bob@in.test", "Bob").await;
    let bob_id = owner_of(&app, "sub-bob-face").await;
    app.fake.set_photo("sub-bob-face", tiny_png(), "image/png");

    // Bob's face through Alice's session: the same 404 as no photo at all,
    // never a hint that Bob has one.
    let face = app.get(&format!("/avatar/{bob_id}"), Some(&alice)).await;
    assert_eq!(face.status, StatusCode::NOT_FOUND);
    assert!(face.bytes.is_empty());

    // And an id that names nobody at all.
    let ghost = app
        .get("/avatar/00000000000000000000000000", Some(&alice))
        .await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);

    // Bob himself still sees his own.
    let own = app.get(&format!("/avatar/{bob_id}"), Some(&bob)).await;
    assert_eq!(own.status, StatusCode::OK);
    assert_eq!(own.bytes, tiny_png());
}

#[tokio::test]
async fn avatar_without_session_is_unauthorized() {
    let app = TestApp::build().await;
    let face = app.get("/avatar/anyone", None).await;
    assert_eq!(face.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn avatar_resolves_local_id_to_oidc_sub() {
    // im keys photos by the OIDC sub, while `/avatar/{id}` carries the
    // local row id — the two are never equal (ULID vs `sub-*`), so a photo
    // map keyed by the sub 404'd before the handler translated the id.
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-looks", "looks@in.test", "Looks").await;
    let me = owner_of(&app, "sub-looks").await;
    assert_ne!(me, "sub-looks", "local id collides with the sub");
    app.fake.set_photo("sub-looks", tiny_png(), "image/png");

    let face = app.get(&format!("/avatar/{me}"), Some(&cookie)).await;
    assert_eq!(face.status, StatusCode::OK);
    assert_eq!(face.bytes, tiny_png());
}

#[tokio::test]
async fn drive_add_menu_has_two_items() {
    let app = TestApp::build().await;
    let cookie = app
        .sign_in("sub-addmenu", "addmenu@in.test", "AddMenu")
        .await;

    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let body = page.text();

    // The filterbar carries one + menu, not the old confirm modal.
    assert!(
        !body.contains("confirm-details"),
        "confirm modal still rendered"
    );
    assert!(
        !body.contains("new-folder-form"),
        "modal form still rendered"
    );
    let details_at = body.find("drive-add").expect("no + menu on the drive");
    let details_end = body[details_at..]
        .find("</details>")
        .expect("menu never closes");
    let details = &body[details_at..details_at + details_end];
    // New folder: a button-only quick form posting just the parent.
    assert!(
        details.contains("action=\"/api/folder/create\""),
        "quick form escapes the menu: {details}"
    );
    assert!(
        details.contains("name=\"parent_id\""),
        "no parent field: {details}"
    );
    assert!(
        !details.contains("name=\"name\""),
        "quick form asks for a name: {details}"
    );
    assert!(
        details.contains("New folder"),
        "no New folder item: {details}"
    );
    // Upload files: a label for the panel's own picker, no script needed.
    assert!(
        details.contains("for=\"drive-upload-input\""),
        "no upload label in the menu: {details}"
    );
    assert!(
        details.contains("Upload files"),
        "no Upload files item: {details}"
    );
    assert!(
        body.contains("id=\"drive-upload-input\""),
        "upload input carries no id for the label"
    );
}

#[tokio::test]
async fn drive_quick_create_names_and_edits() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-quick", "quick@in.test", "Quick").await;
    let me = owner_of(&app, "sub-quick").await;

    // The + menu posts no name: generic name, 303 into that row's edit mode.
    let answer = app
        .post("/api/folder/create", Some(&cookie), &[("parent_id", "")])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let location = answer
        .location
        .clone()
        .expect("quick create redirects nowhere");
    assert!(
        location.starts_with("/drive?"),
        "quick create leaves the drive: {location}"
    );
    assert!(
        location.contains("edit="),
        "quick create skips edit mode: {location}"
    );
    let edit = location
        .split("edit=")
        .nth(1)
        .expect("no edit id")
        .split('&')
        .next()
        .unwrap();
    assert_eq!(folder_id(&app, &me, None, "New folder").await, edit);

    // A second quick create never refuses: the store postfixes the generic
    // name instead of duplicating it.
    let answer = app
        .post("/api/folder/create", Some(&cookie), &[("parent_id", "")])
        .await;
    let location = answer
        .location
        .clone()
        .expect("second quick create redirects nowhere");
    let edit2 = location
        .split("edit=")
        .nth(1)
        .expect("no edit id")
        .split('&')
        .next()
        .unwrap();
    assert_ne!(edit, edit2, "second quick create reused the row");
    let root = app.store.list_children(&me, None).await.unwrap();
    let names: Vec<&str> = root
        .folders
        .iter()
        .map(|folder| folder.name.as_str())
        .collect();
    assert!(
        names.contains(&"New folder") && names.contains(&"New folder (2)"),
        "second quick create did not postfix the generic name: {names:?}"
    );

    // A typed name keeps the old answer: back to the page, no edit mode.
    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "typed")],
        )
        .await;
    assert!(
        answer.accepted(),
        "typed create refused: {:?}",
        answer.location
    );
    assert!(
        answer
            .location
            .as_deref()
            .is_some_and(|location| !location.contains("edit=")),
        "typed create enters edit mode: {:?}",
        answer.location
    );
    folder_id(&app, &me, None, "typed").await;

    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "typed")],
        )
        .await;
    assert!(
        answer.accepted(),
        "typed dupe refused: {:?}",
        answer.location
    );
    let root = app.store.list_children(&me, None).await.unwrap();
    let names: Vec<&str> = root
        .folders
        .iter()
        .map(|folder| folder.name.as_str())
        .collect();
    assert!(
        names.contains(&"typed") && names.contains(&"typed (2)"),
        "typed dupe did not land postfixed: {names:?}"
    );
}

#[tokio::test]
async fn drive_edit_row_renders_the_rename_form() {
    let app = TestApp::build().await;
    let cookie = app
        .sign_in("sub-editrow", "editrow@in.test", "EditRow")
        .await;
    let me = owner_of(&app, "sub-editrow").await;

    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "plans")],
        )
        .await;
    assert!(
        answer.accepted(),
        "setup create refused: {:?}",
        answer.location
    );
    let id = folder_id(&app, &me, None, "plans").await;

    // Plain view: the row is a link with an edit entry, no rename input.
    let page = app.get("/drive", Some(&cookie)).await;
    let body = page.text();
    assert!(
        body.contains(&format!("/drive?folder={id}")),
        "row links nowhere"
    );
    assert!(
        body.contains(&format!("/drive?edit={id}")),
        "row has no edit entry"
    );

    // Edit view: that row — and only that row — is the rename form, pre-filled.
    let page = app.get(&format!("/drive?edit={id}"), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    let body = page.text();
    assert!(
        body.contains("action=\"/api/folder/rename\""),
        "edit view renders no rename form"
    );
    assert!(
        body.contains("data-edit-focus"),
        "rename input carries no focus hook"
    );
    assert!(
        body.contains("value=\"plans\""),
        "rename input is not pre-filled"
    );
    assert!(body.contains("__inEditFocus"), "no edit focus script");
}

#[tokio::test]
async fn drive_lists_folders_before_files_in_one_panel() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-merged", "merged@in.test", "Merged").await;
    let owner = owner_of(&app, "sub-merged").await;
    // The file's name sorts first alphabetically, so order proves folders lead.
    let file_id = file_id(&app, &owner, "aaa file.txt", b"a").await;
    let answer = app
        .post(
            "/api/folder/create",
            Some(&cookie),
            &[("parent_id", ""), ("name", "mmm folder")],
        )
        .await;
    assert!(
        answer.accepted(),
        "setup create refused: {:?}",
        answer.location
    );

    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    // One panel, no heads or chips — just the merged list.
    assert_eq!(
        body.matches("<section class=\"panel drive-panel\">")
            .count(),
        1,
        "{body}"
    );
    assert!(!body.contains("panel-head"), "{body}");
    assert!(!body.contains("class=\"chip\""), "{body}");
    assert!(!body.contains(">Folders</h2>"), "{body}");
    assert!(!body.contains(">Files</h2>"), "{body}");
    // Folders first, then files, each wired to its surface.
    let folder_at = body.find("mmm folder").expect("no folder row");
    let file_at = body.find("aaa file.txt").expect("no file row");
    assert!(folder_at < file_at, "file row precedes folder row: {body}");
    // Move is a picker modal now: the row's menu links to it, and the modal
    // carries the destination rows that post the move.
    let folder_id = {
        let at = body.find("/drive?folder=").expect("no folder link");
        let rest = &body[at + "/drive?folder=".len()..];
        let end = rest.find('"').expect("unterminated folder link");
        rest[..end].to_string()
    };
    assert!(
        body.contains(&format!("move=folder:{folder_id}")),
        "folder row has no move entry: {body}"
    );
    assert!(
        body.contains(&format!("move=file:{file_id}")),
        "file row has no move entry: {body}"
    );
    let picker = app
        .get(&format!("/drive?move=folder:{folder_id}"), Some(&cookie))
        .await;
    let picker_body = picker.text();
    assert!(
        picker_body.contains("action=\"/api/folder/move\""),
        "{picker_body}"
    );
    // A folder never offers its own subtree as a destination: with only the
    // target folder in the tree, the drive root is the one row left.
    assert_eq!(
        picker_body.matches("class=\"move-pick\"").count(),
        1,
        "{picker_body}"
    );
    // File rename follows the folder pattern: an edit link on the plain
    // view, the form only on the edit view.
    assert!(
        body.contains(&format!("/drive?edit={file_id}")),
        "file row has no edit entry: {body}"
    );
    let edit_page = app
        .get(&format!("/drive?edit={file_id}"), Some(&cookie))
        .await;
    assert!(
        edit_page.text().contains("action=\"/api/file/rename\""),
        "{}",
        edit_page.text()
    );
    // A non-empty list carries no empty state.
    assert!(!body.contains("This folder is empty"), "{body}");
}

#[tokio::test]
async fn an_empty_folder_says_how_to_fill_it() {
    let app = TestApp::build().await;
    let cookie = app.sign_in("sub-empty", "empty@in.test", "Empty").await;
    let page = app.get("/drive", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    let body = page.text();
    // The single empty state names both ways to fill the folder.
    assert!(
        body.contains(
            "This folder is empty. Press + to create or upload, or drop files on this page."
        ),
        "no empty-state hint: {body}"
    );
    assert_eq!(
        body.matches("<section class=\"panel drive-panel\">")
            .count(),
        1,
        "{body}"
    );
    assert!(!body.contains("panel-head"), "{body}");
    // And the upload control plus the drop handler the hint promises are on the page.
    assert!(body.contains("id=\"upload-form\""), "{body}");
    assert!(body.contains("__inDrop"), "{body}");
}
