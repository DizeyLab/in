//! Development helper: mints a session-cookie value for smoke runs against a
//! fake im, without a browser dance. Requires the `test-seam` feature.
//!
//! ```text
//! cargo run -p in-client --example mint --features test-seam -- \
//!     <path-to-in.key> <app-session-token> [cookie-name]
//! ```
//!
//! Print the value, then hand it to the browser as the session cookie. Any
//! token string works as long as the fake provider's introspection answers
//! `active: true` for it.

fn main() {
    let mut args = std::env::args().skip(1);
    let key_path = args.next().unwrap_or_else(|| {
        eprintln!("usage: mint <in.key path> <app-session-token> [cookie-name]");
        std::process::exit(2);
    });
    let token = args.next().expect("token");
    let cookie_name = args.next().unwrap_or_else(|| "in_session".to_string());
    let key: [u8; 32] = std::fs::read(&key_path)
        .unwrap_or_else(|e| panic!("{key_path}: {e}"))
        .try_into()
        .expect("key file is not 32 bytes");
    let config = in_client::Config {
        issuer: String::new(),
        client_id: String::new(),
        client_secret: String::new(),
        redirect_uri: String::new(),
        cookie_name,
        cookie_key: key,
    };
    let exp = time::OffsetDateTime::now_utc() + time::Duration::days(1);
    println!("{}", in_client::mint_session_cookie(&config, &token, exp));
}
