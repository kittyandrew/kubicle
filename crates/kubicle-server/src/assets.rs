//! Caching of the app's own files: the client the Nix build puts in `KUBICLE_WEB_DIR` and the system themes.
//!
//! Every file there comes from the Nix store, its mtimes all 1970: with only `Last-Modified`, browsers guess years
//! of freshness and keep the old client after a deploy. So the build names the client's files after their content
//! (`kubicle_web_bg.<16 hex>.wasm`, see nix/package.nix), and themes carry theirs as `?v=<16 hex>`: both are cached
//! for good. All else (`index.html`, the manifest, icons) keeps the router's `no-store` (http.rs, `router`): the
//! next load gets the new one.

use std::path::Path;

use axum::{extract::Request, http::HeaderValue, http::header, middleware::Next, response::Response};
use ring::digest::SHA256;

/// The hex digits of the content hash in a hashed file name.
const HASH_LEN: usize = 16;

/// The first `HASH_LEN` hex digits of the SHA-256 of `bytes`, as nix/package.nix names files with `sha256sum`.
pub fn content_hash(bytes: &[u8]) -> String {
    ring::digest::digest(&SHA256, bytes).as_ref()[..HASH_LEN / 2].iter().fold(String::new(), |s, b| s + &format!("{b:02x}"))
}

/// The client build in `web_dir`: the name of its content-named wasm (`kubicle_web_bg.<hash>.wasm`), which the
/// build also writes into boot.js; "dev" for an unhashed development build, as boot.js says then too.
pub fn build(web_dir: &Path) -> String {
    let names = std::fs::read_dir(web_dir).into_iter().flatten().filter_map(|e| e.ok()?.file_name().into_string().ok());
    names.filter(|n| n.starts_with("kubicle_web_bg.") && hashed(n)).min().unwrap_or_else(|| "dev".into())
}

/// A query naming its content's version, `v=<hash>`: the theme addresses `ThemeInfo::version` makes.
pub fn versioned(query: Option<&str>) -> bool { query.is_some_and(|q| q.split('&').any(|pair| pair.starts_with("v="))) }

/// A file named `<stem>.<HASH_LEN hex>.<ext>`.
fn hashed(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut parts = name.rsplitn(3, '.');
    let (_ext, hash, stem) = (parts.next(), parts.next(), parts.next());
    stem.is_some() && hash.is_some_and(|h| h.len() == HASH_LEN && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Middleware over the file services: hashed and versioned files are immutable.
pub async fn headers(req: Request, next: Next) -> Response {
    let fixed = hashed(req.uri().path()) || versioned(req.uri().query());
    let mut response = next.run(req).await;
    if fixed && response.status().is_success() {
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    }
    response
}
