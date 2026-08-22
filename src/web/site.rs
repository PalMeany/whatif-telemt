//! Operator-owned static site served entirely from memory.
//!
//! Every path — assets, the index, and the 404 body — is answered by one code
//! path with one header set. Serving some paths from disk and others from
//! memory is an active-probing tell, so there is deliberately only one path.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;

use crate::error::{ProxyError, Result};

/// Uniform security header set applied to the whole static surface.
pub(crate) const SITE_CSP: &str = "default-src 'self'; style-src 'self'; img-src 'self'; worker-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// One in-memory file of the public site.
pub(crate) struct StaticEntry {
    pub(crate) body: Bytes,
    pub(crate) content_type: &'static str,
}

/// The whole public site, loaded once at start-up.
pub(crate) struct StaticSite {
    entries: HashMap<String, Arc<StaticEntry>>,
    index: Arc<StaticEntry>,
    not_found: Arc<StaticEntry>,
    last_modified: String,
    modified_at: SystemTime,
}

impl StaticSite {
    /// Reads every regular file under `root` into memory.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        let mut newest = SystemTime::UNIX_EPOCH;
        collect(root, root, &mut entries, &mut newest)?;
        let index = entries
            .get("/index.html")
            .cloned()
            .ok_or_else(|| ProxyError::Config("web.public_dir must contain index.html".into()))?;
        let not_found = entries
            .get("/404.html")
            .cloned()
            .unwrap_or_else(|| index.clone());
        let modified_at = if newest == SystemTime::UNIX_EPOCH {
            SystemTime::now()
        } else {
            newest
        };
        Ok(Self {
            entries,
            index,
            not_found,
            last_modified: httpdate::fmt_http_date(modified_at),
            modified_at,
        })
    }

    /// The site index, also used for every non-capability root request.
    pub(crate) fn index(&self) -> &Arc<StaticEntry> {
        &self.index
    }

    /// The site 404 body, used for every unauthenticated relay response.
    pub(crate) fn not_found(&self) -> &Arc<StaticEntry> {
        &self.not_found
    }

    /// `Last-Modified` value shared by every entry.
    pub(crate) fn last_modified(&self) -> &str {
        &self.last_modified
    }

    /// True when the client's `If-Modified-Since` still covers the site.
    pub(crate) fn not_modified(&self, header: Option<&str>) -> bool {
        let Some(value) = header else {
            return false;
        };
        let Ok(since) = httpdate::parse_http_date(value) else {
            return false;
        };
        // Truncate to whole seconds: HTTP dates carry no sub-second precision.
        let truncated = truncate_to_seconds(self.modified_at);
        truncated <= since
    }

    /// Resolves a request path the way an ordinary static server does.
    ///
    /// The path is percent-decoded first, then matched exactly, then as a
    /// directory index, then `/favicon.ico` to `favicon.svg`, then `{path}.html`
    /// for an extensionless request. It never falls back to the index for
    /// arbitrary paths.
    ///
    /// Decoding and directory indexes are not conveniences here. Every real
    /// static server serves `/%69ndex.html` and `/blog/`, so answering 404 to
    /// either is a difference between this origin and every other one, which is
    /// exactly what an active prober compares.
    pub(crate) fn resolve(&self, request_path: &str) -> Option<&Arc<StaticEntry>> {
        let decoded = percent_decode(request_path)?;
        let path = decoded.as_str();
        if !path.starts_with('/') || !is_clean_path(path) {
            return None;
        }
        if let Some(entry) = self.entries.get(path) {
            return Some(entry);
        }
        if path.ends_with('/') {
            let candidate = format!("{path}index.html");
            return self.entries.get(&candidate);
        }
        if path == "/favicon.ico" {
            return self.entries.get("/favicon.svg");
        }
        if extension_of(path).is_none() {
            let candidate = format!("{path}.html");
            return self.entries.get(&candidate);
        }
        None
    }
}

/// Percent-decodes a request path, refusing anything a server would not serve.
///
/// A decoded byte that is not valid UTF-8, a NUL, or any other control
/// character is refused outright rather than normalised, so the decoded form
/// can be matched against the site map without a second escaping rule.
fn percent_decode(value: &str) -> Option<String> {
    if !value.contains('%') {
        return Some(value.to_string());
    }
    let raw = value.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] != b'%' {
            out.push(raw[index]);
            index += 1;
            continue;
        }
        if index + 2 >= raw.len() {
            return None;
        }
        let high = (raw[index + 1] as char).to_digit(16)?;
        let low = (raw[index + 2] as char).to_digit(16)?;
        let byte = (high * 16 + low) as u8;
        if byte < 0x20 || byte == 0x7f {
            return None;
        }
        out.push(byte);
        index += 3;
    }
    String::from_utf8(out).ok()
}

/// Cache policy shared by every entry, independent of the bridge capability.
///
/// Anything with a query string or any 4xx is uncacheable; everything else is
/// cacheable, so `/?bridge=x` and `/about?x` receive the same header.
pub(crate) fn cache_control(query: Option<&str>, status: u16) -> &'static str {
    if status >= 400 || query.is_some_and(|value| !value.is_empty()) {
        "no-store"
    } else {
        "public, max-age=300"
    }
}

fn truncate_to_seconds(value: SystemTime) -> SystemTime {
    match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(elapsed.as_secs()),
        Err(_) => value,
    }
}

/// Rejects any path a canonicalizing cleaner would rewrite.
///
/// A single trailing slash is allowed, because it is the directory-index form
/// `resolve` handles; `//` and the `.`/`..` segments are still refused, which
/// is what keeps a percent-encoded traversal from reaching the site map.
fn is_clean_path(value: &str) -> bool {
    if value.contains('\\') || value.contains("//") {
        return false;
    }
    !value
        .split('/')
        .any(|segment| segment == "." || segment == "..")
}

fn extension_of(value: &str) -> Option<&str> {
    let file = value.rsplit('/').next()?;
    let (_, extension) = file.rsplit_once('.')?;
    (!extension.is_empty()).then_some(extension)
}

fn collect(
    root: &Path,
    directory: &Path,
    entries: &mut HashMap<String, Arc<StaticEntry>>,
    newest: &mut SystemTime,
) -> Result<()> {
    let listing = std::fs::read_dir(directory)
        .map_err(|error| ProxyError::Config(format!("web.public_dir: {error}")))?;
    for item in listing {
        let item = item.map_err(|error| ProxyError::Config(format!("web.public_dir: {error}")))?;
        let path = item.path();
        let file_type = item
            .file_type()
            .map_err(|error| ProxyError::Config(format!("web.public_dir: {error}")))?;
        if file_type.is_dir() {
            collect(root, &path, entries, newest)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(relative) = relative_url(root, &path) else {
            continue;
        };
        let body = std::fs::read(&path)
            .map_err(|error| ProxyError::Config(format!("web.public_dir: {error}")))?;
        if let Ok(metadata) = item.metadata()
            && let Ok(modified) = metadata.modified()
            && modified > *newest
        {
            *newest = modified;
        }
        entries.insert(
            relative,
            Arc::new(StaticEntry {
                body: Bytes::from(body),
                content_type: content_type_for(&path),
            }),
        );
    }
    Ok(())
}

fn relative_url(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut url = String::from("/");
    for (index, component) in relative.components().enumerate() {
        let text = component.as_os_str().to_str()?;
        if index != 0 {
            url.push('/');
        }
        url.push_str(text);
    }
    Some(url)
}

fn content_type_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_site() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"index").expect("index");
        std::fs::write(dir.path().join("about.html"), b"about").expect("about");
        std::fs::write(dir.path().join("404.html"), b"missing").expect("404");
        std::fs::write(dir.path().join("favicon.svg"), b"<svg/>").expect("favicon");
        std::fs::create_dir(dir.path().join("assets")).expect("assets");
        std::fs::write(dir.path().join("assets/app.css"), b"body{}").expect("css");
        dir
    }

    #[test]
    fn resolves_clean_links_and_favicon() {
        let dir = write_site();
        let site = StaticSite::load(dir.path()).expect("load");
        assert_eq!(site.resolve("/index.html").map(|e| e.body.len()), Some(5));
        assert_eq!(site.resolve("/about").map(|e| e.body.len()), Some(5));
        assert_eq!(site.resolve("/favicon.ico").map(|e| e.body.len()), Some(6));
        assert_eq!(
            site.resolve("/assets/app.css").map(|e| e.content_type),
            Some("text/css; charset=utf-8")
        );
        assert!(site.resolve("/missing").is_none());
    }

    #[test]
    fn rejects_traversal_and_dirty_paths() {
        let dir = write_site();
        let site = StaticSite::load(dir.path()).expect("load");
        assert!(site.resolve("/../index.html").is_none());
        assert!(site.resolve("//index.html").is_none());
        assert!(site.resolve("/./index.html").is_none());
        assert!(site.resolve("index.html").is_none());
        // Traversal survives decoding, so it is refused after decoding too.
        assert!(site.resolve("/%2e%2e/index.html").is_none());
        assert!(site.resolve("/assets%2f..%2findex.html").is_none());
        assert!(site.resolve("/index.html%00").is_none());
        assert!(site.resolve("/index.html%zz").is_none());
        assert!(site.resolve("/index.html%2").is_none());
    }

    #[test]
    fn resolves_percent_encoded_and_directory_paths() {
        let dir = write_site();
        std::fs::create_dir(dir.path().join("blog")).expect("blog");
        std::fs::write(dir.path().join("blog/index.html"), b"posts").expect("blog index");
        let site = StaticSite::load(dir.path()).expect("load");
        assert_eq!(site.resolve("/%69ndex.html").map(|e| e.body.len()), Some(5));
        assert_eq!(site.resolve("/blog/").map(|e| e.body.len()), Some(5));
        assert_eq!(site.resolve("/").map(|e| e.body.len()), Some(5));
        assert!(site.resolve("/nothing/").is_none());
    }

    #[test]
    fn cache_policy_ignores_capability_shape() {
        assert_eq!(cache_control(None, 200), "public, max-age=300");
        assert_eq!(cache_control(Some("bridge=x"), 200), "no-store");
        assert_eq!(cache_control(None, 404), "no-store");
    }

    #[test]
    fn missing_index_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("about.html"), b"about").expect("about");
        assert!(StaticSite::load(dir.path()).is_err());
    }
}
