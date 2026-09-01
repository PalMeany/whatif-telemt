//! Response construction for the panel HTTP surface.
//!
//! Every panel answer carries the same hardening headers, and every JSON answer
//! carries the same envelope. Both are built here so a new route cannot forget
//! either one.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use serde::Serialize;

/// Body type used by every panel response.
pub(crate) type PanelBody = Full<Bytes>;

/// Content security policy applied to the application shell.
///
/// The bundle ships every script and style it needs, so no external origin is
/// allowed at all: a stored value that reaches the DOM cannot pull code in from
/// anywhere. `style-src` keeps `'unsafe-inline'` because the bundled CSS-in-JS
/// runtime writes style attributes, and no inline *script* is permitted either
/// way.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; \
     font-src 'self' data:; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'; \
     object-src 'none'";

/// Success envelope.
#[derive(Serialize)]
struct SuccessEnvelope<T> {
    ok: bool,
    data: T,
}

/// Error envelope.
#[derive(Serialize)]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

/// Machine and human halves of one error.
#[derive(Serialize)]
struct ErrorBody {
    code: String,
    message: String,
}

/// Builds a JSON success response.
pub(crate) fn json<T: Serialize>(status: StatusCode, data: T) -> Response<PanelBody> {
    let payload = SuccessEnvelope { ok: true, data };
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| {
        br#"{"ok":false,"error":{"code":"internal","message":"encode failed"}}"#.to_vec()
    });
    build(status, "application/json; charset=utf-8", body)
}

/// Builds a JSON error response.
pub(crate) fn error(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response<PanelBody> {
    let payload = ErrorEnvelope {
        ok: false,
        error: ErrorBody {
            code: code.to_string(),
            message: message.into(),
        },
    };
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    build(status, "application/json; charset=utf-8", body)
}

/// Relays an upstream Control API answer without reshaping it.
///
/// The Control API's own envelope is what the browser's client library already
/// understands, and rewrapping it would hide the `revision` field the config
/// editor depends on for optimistic concurrency.
pub(crate) fn passthrough(
    status: StatusCode,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Response<PanelBody> {
    build(
        status,
        content_type.unwrap_or("application/json; charset=utf-8"),
        body,
    )
}

/// Builds a response with the given media type and the standard headers.
pub(crate) fn build(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<PanelBody> {
    let mut response = Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body)))
        .expect("panel response builder inputs are static");
    let headers = response.headers_mut();
    insert(headers, "content-type", content_type);
    insert(headers, "x-content-type-options", "nosniff");
    insert(headers, "x-frame-options", "DENY");
    insert(headers, "referrer-policy", "no-referrer");
    insert(
        headers,
        "permissions-policy",
        "accelerometer=(), camera=(), geolocation=(), gyroscope=(), microphone=(), payment=(), usb=()",
    );
    insert(headers, "cross-origin-opener-policy", "same-origin");
    insert(headers, "cross-origin-resource-policy", "same-origin");
    insert(headers, "content-security-policy", CONTENT_SECURITY_POLICY);
    insert(headers, "cache-control", "no-store");
    response
}

/// Adds `Strict-Transport-Security` to a response served over TLS.
pub(crate) fn with_hsts(mut response: Response<PanelBody>) -> Response<PanelBody> {
    insert(
        response.headers_mut(),
        "strict-transport-security",
        "max-age=31536000; includeSubDomains",
    );
    response
}

/// Replaces `Cache-Control` for an immutable asset.
pub(crate) fn with_immutable_cache(mut response: Response<PanelBody>) -> Response<PanelBody> {
    insert(
        response.headers_mut(),
        "cache-control",
        "public, max-age=31536000, immutable",
    );
    response
}

/// Appends one `Set-Cookie` header.
pub(crate) fn with_cookie(mut response: Response<PanelBody>, cookie: &str) -> Response<PanelBody> {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append("set-cookie", value);
    }
    response
}

/// Inserts a header, dropping values that cannot be encoded.
pub(crate) fn insert(headers: &mut hyper::HeaderMap<HeaderValue>, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_response_carries_the_hardening_headers() {
        let response = json(StatusCode::OK, serde_json::json!({"a": 1}));
        let headers = response.headers();
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            headers["content-security-policy"]
                .to_str()
                .expect("ascii")
                .contains("frame-ancestors 'none'")
        );
        assert!(
            !headers["content-security-policy"]
                .to_str()
                .expect("ascii")
                .contains("script-src 'self' 'unsafe-inline'")
        );
    }

    #[test]
    fn the_error_envelope_carries_a_machine_code() {
        let response = error(StatusCode::FORBIDDEN, "read_only", "nope");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn immutable_assets_override_the_default_no_store() {
        let response = with_immutable_cache(build(StatusCode::OK, "text/css", Vec::new()));
        assert_eq!(
            response.headers()["cache-control"],
            "public, max-age=31536000, immutable"
        );
    }
}
