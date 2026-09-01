//! The embedded single-page application.
//!
//! The bundle is baked into the binary by `build.rs`, so a panel deployment is
//! still one file to copy. Content-hashed assets are served with a long
//! immutable cache; `index.html` never is, because it is what points at the
//! current hashes.

use hyper::StatusCode;

use super::respond::{self, PanelBody};

include!(concat!(env!("OUT_DIR"), "/panel_assets.rs"));

/// Page served when the binary was built without a bundle.
///
/// This is an operational state, not a placeholder: a source build that skipped
/// the UI step still has a working API, and saying so beats a blank 404.
const MISSING_BUNDLE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>telemt panel</title></head><body style=\"font-family:ui-monospace,monospace;background:#000;color:#fff;padding:2rem\">\
<h1>Panel bundle not built</h1><p>This binary was compiled without <code>panel-ui/dist</code>. \
Run <code>npm --prefix panel-ui ci &amp;&amp; npm --prefix panel-ui run build</code> and rebuild.</p>\
<p>The panel API is unaffected and remains available under <code>/panel/api</code>.</p></body></html>";

/// Serves one asset by its route, or the application shell.
///
/// Unknown paths fall through to `index.html` so the client-side router owns
/// deep links; a request that looks like a file is answered with 404 instead,
/// because serving HTML for a missing script only produces a confusing parse
/// error in the browser console.
pub(crate) fn serve(path: &str) -> hyper::Response<PanelBody> {
    let route = path.trim_start_matches('/');
    if let Some(asset) = lookup(route) {
        let response = respond::build(StatusCode::OK, asset.1, asset.2.to_vec());
        return if is_hashed(route) {
            respond::with_immutable_cache(response)
        } else {
            response
        };
    }
    if looks_like_file(route) {
        return respond::error(StatusCode::NOT_FOUND, "not_found", "Asset not found");
    }
    index()
}

/// Serves the application shell.
pub(crate) fn index() -> hyper::Response<PanelBody> {
    match lookup("index.html") {
        Some(asset) => respond::build(StatusCode::OK, asset.1, asset.2.to_vec()),
        None => respond::build(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/html; charset=utf-8",
            MISSING_BUNDLE.as_bytes().to_vec(),
        ),
    }
}

/// True when the binary carries a bundle.
pub(crate) fn is_bundled() -> bool {
    lookup("index.html").is_some()
}

/// Finds one asset by route.
fn lookup(route: &str) -> Option<&'static (&'static str, &'static str, &'static [u8])> {
    let route = if route.is_empty() {
        "index.html"
    } else {
        route
    };
    PANEL_ASSETS.iter().find(|asset| asset.0 == route)
}

/// True when the route carries a bundler content hash.
fn is_hashed(route: &str) -> bool {
    let Some(name) = route.rsplit('/').next() else {
        return false;
    };
    // Vite emits `name-<hash>.ext` where the hash is base64url, so `_` and `-`
    // belong to its alphabet. Anything else is served without a long cache so a
    // rebuild is picked up immediately.
    name.rsplit_once('.')
        .map(|(stem, _)| stem)
        .and_then(|stem| stem.rsplit_once('-'))
        .map(|(_, hash)| {
            hash.len() >= 8
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .unwrap_or(false)
}

/// True when the route names a file rather than a client-side page.
fn looks_like_file(route: &str) -> bool {
    route
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_assets_are_recognised() {
        assert!(is_hashed("assets/index-a1b2c3d4.js"));
        // The bundler's hash alphabet is base64url, so these are hashes too.
        assert!(is_hashed("assets/index-y__qiixw.css"));
        assert!(is_hashed("assets/index-BV6qyri7.js"));
        assert!(!is_hashed("index.html"));
        assert!(!is_hashed("assets/logo.svg"));
        assert!(!is_hashed("assets/logo-dark.svg"));
    }

    #[test]
    fn client_side_routes_fall_through_to_the_shell() {
        assert!(!looks_like_file("nodes"));
        assert!(!looks_like_file("users/alice"));
        assert!(looks_like_file("assets/index-a1b2c3d4.js"));
        assert!(looks_like_file("favicon.ico"));
    }

    #[test]
    fn a_missing_asset_answers_404_rather_than_html() {
        let response = serve("/assets/does-not-exist.js");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
