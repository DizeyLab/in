//! Compiles `style/main.scss` to `assets/main.css` under this crate's
//! manifest directory, where `asset!("assets/main.css")` in `layout.rs`
//! expects to find it. Crate-relative rather than `OUT_DIR`-relative so the
//! asset's id does not churn with every profile/build-hash (see the topcoat
//! contract sheet, (a)).
//!
use sha2::Digest;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let scss = format!("{manifest_dir}/../../style/main.scss");
    println!("cargo:rerun-if-changed={scss}");
    // Partials pulled in via @use — without these, editing a skin file would
    // leave assets/main.css stale.
    println!("cargo:rerun-if-changed={manifest_dir}/../../style/_instrument.scss");
    println!("cargo:rerun-if-changed={manifest_dir}/../../style/_ledger.scss");

    let css = grass::from_path(&scss, &grass::Options::default())
        .unwrap_or_else(|err| panic!("failed to compile {scss}: {err}"));
    let out_dir = format!("{manifest_dir}/assets");
    std::fs::create_dir_all(&out_dir).expect("failed to create assets/");
    std::fs::write(format!("{out_dir}/main.css"), &css).expect("failed to write assets/main.css");

    // The binary carries the compiled stylesheet's fingerprint, so the
    // running server can tell a bundle of its own generation from one left
    // behind by another deploy (`server.rs`'s `stylesheet_guard` refuses the
    // boot on a mismatch). The bytes hashed here are exactly the bytes the
    // bundler later copies into the bundle, so the fingerprints agree.
    let fingerprint = sha2::Sha256::digest(css.as_bytes());
    let mut hex = String::with_capacity(fingerprint.len() * 2);
    for byte in fingerprint {
        hex.push_str(&format!("{byte:02x}"));
    }
    println!("cargo:rustc-env=IN_STYLE_FINGERPRINT=sha256:{hex}");

    // The commit this binary was built from, served by `/healthz` and
    // asserted against after the deploy restart, so a stale process
    // holding the port cannot pass for this deploy. "dev" locally, where
    // nothing asserts it. The rerun-if-env-changed line is load-bearing:
    // without it a rust-cache hit would ship last deploy's sha.
    let sha = std::env::var("IN_BUILD_SHA").unwrap_or_else(|_| "dev".into());
    println!("cargo:rustc-env=IN_BUILD_SHA={sha}");
    println!("cargo:rerun-if-env-changed=IN_BUILD_SHA");
}
