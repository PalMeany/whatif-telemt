//! The built-in Prometheus panel.
//!
//! One self-contained HTML document served next to the exposition this process
//! already renders. It carries no external reference of any kind: the page
//! scrapes `/metrics` from its own origin and draws it client-side, so it works
//! on a host with no outbound network and adds no dependency to the binary.
//!
//! Deliberately not a Grafana replacement. It answers the question an operator
//! has while logged into the box — is the proxy healthy right now — without
//! standing up a scrape target and a dashboard first.
//!
//! Submodules:
//! - `listener`: the optional dedicated panel listener

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use crate::config::ProxyConfig;
use crate::crypto::SecureRandom;

pub(crate) mod listener;

/// Panel document, with placeholders substituted per response.
const DOCUMENT: &str = include_str!("panel.html");

/// Bytes of nonce material used for the page's script and style nonce.
const NONCE_BYTES: usize = 18;

/// Path the page scrapes. Always the exposition this process renders.
pub(super) const METRICS_PATH: &str = "/metrics";

/// Heading used when the operator did not choose one.
const DEFAULT_TITLE: &str = "telemt";

/// Reports whether a request path is the configured panel path.
pub(crate) fn is_panel_path(config: &ProxyConfig, path: &str) -> bool {
    config.fork.prometheus_enabled() && path == config.fork.prometheus.path
}

/// Renders the panel for one request.
///
/// The page is generated per response rather than cached because its nonce
/// must not be reused across responses, and the document is small enough that
/// the substitution is not worth a cache.
pub(crate) fn render(config: &ProxyConfig, rng: &SecureRandom) -> Response<Full<Bytes>> {
    let panel = &config.fork.prometheus;
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    rng.fill(&mut nonce_bytes);
    let nonce = URL_SAFE_NO_PAD.encode(nonce_bytes);

    let title = if panel.title.trim().is_empty() {
        format!("{} {}", DEFAULT_TITLE, crate::VERSION)
    } else {
        panel.title.clone()
    };
    let settings = serde_json::json!({
        "metricsPath": METRICS_PATH,
        "refreshSeconds": panel.refresh_secs,
        "historyPoints": panel.history_points,
        "showUsers": panel.show_users,
    });

    let body = DOCUMENT
        .replace("__NONCE__", &nonce)
        .replace("__TITLE__", &escape_html(&title))
        .replace("__CONFIG__", &settings.to_string());

    // Fail closed rather than serve a page with a literal placeholder in it:
    // a surviving `__NONCE__` would mean the script is blocked by the policy
    // below and the page silently renders nothing.
    if body.contains("__NONCE__") || body.contains("__TITLE__") || body.contains("__CONFIG__") {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                b"panel template substitution failed\n",
            )))
            .expect("a static error response is always well formed");
    }

    let csp = [
        "default-src 'none'",
        "base-uri 'none'",
        "connect-src 'self'",
        "form-action 'none'",
        "frame-ancestors 'none'",
        "img-src 'none'",
        "object-src 'none'",
        &format!("script-src 'nonce-{nonce}'"),
        &format!("style-src 'nonce-{nonce}'"),
    ]
    .join("; ");

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("content-security-policy", csp)
        .header("referrer-policy", "no-referrer")
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("a rendered panel response is always well formed")
}

/// Escapes the operator-supplied title for both text and attribute contexts.
fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copies a rendered body out of the response for assertion.
    fn body_bytes(response: &Response<Full<Bytes>>) -> Vec<u8> {
        use http_body_util::BodyExt;
        futures::executor::block_on(async {
            response
                .body()
                .clone()
                .collect()
                .await
                .expect("a full body never fails to collect")
                .to_bytes()
                .to_vec()
        })
    }

    fn enabled_config() -> ProxyConfig {
        let mut config = ProxyConfig::default();
        config.fork.prometheus.enabled = true;
        config
    }

    #[test]
    fn the_panel_path_is_only_matched_while_the_panel_is_enabled() {
        let mut config = enabled_config();
        assert!(is_panel_path(&config, "/panel"));
        assert!(!is_panel_path(&config, "/metrics"));

        config.fork.prometheus.enabled = false;
        assert!(!is_panel_path(&config, "/panel"));
    }

    #[test]
    fn the_master_switch_takes_the_panel_path_with_it() {
        let mut config = enabled_config();
        config.fork.enabled = false;
        assert!(!is_panel_path(&config, "/panel"));
    }

    /// Every metric family the panel document reads.
    ///
    /// Pinned here because the page falls back silently on a missing name, and
    /// a silent zero reads as "the proxy is idle" rather than as a rename.
    const PANEL_SERIES: &[&str] = &[
        "telemt_uptime_seconds",
        "telemt_connections_total",
        "telemt_connections_bad_total",
        "telemt_connections_bad_by_class_total",
        "telemt_direct_relay_buffer_sessions",
        "telemt_ip_tracker_entries",
        "telemt_ip_tracker_users",
        "telemt_me_writers_active_current",
        "telemt_me_writers_warm_current",
        "telemt_me_reconnect_attempts_total",
        "telemt_me_keepalive_sent_total",
        "telemt_me_keepalive_timeout_total",
        "telemt_me_handshake_reject_total",
        "telemt_me_d2c_payload_bytes_total",
        "telemt_buffer_pool_buffers_total",
        "telemt_build_info",
        "telemt_telemetry_core_enabled",
        "telemt_user_connections_current",
        "telemt_user_unique_ips_current",
        "telemt_user_octets_from_client_total",
        "telemt_user_octets_to_client_total",
    ];

    #[test]
    fn every_series_the_panel_reads_is_one_this_binary_emits() {
        let exporter = include_str!("../../metrics.rs");
        for series in PANEL_SERIES {
            assert!(
                exporter.contains(series),
                "the panel reads `{series}`, which src/metrics.rs does not emit"
            );
        }
    }

    #[test]
    fn the_panel_reads_no_series_outside_that_list() {
        // Catches a name added to the page without being checked against the
        // exporter, which is how the page starts drawing silent zeroes.
        let mut cursor = DOCUMENT;
        while let Some(start) = cursor.find("telemt_") {
            cursor = &cursor[start..];
            let end = cursor
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(cursor.len());
            let name = &cursor[..end];
            assert!(
                PANEL_SERIES.contains(&name),
                "the panel reads `{name}`, which is not in PANEL_SERIES"
            );
            cursor = &cursor[end..];
        }
    }

    #[test]
    fn every_placeholder_is_substituted() {
        let response = render(&enabled_config(), &SecureRandom::new());
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(body_bytes(&response)).unwrap();
        assert!(!body.contains("__NONCE__"));
        assert!(!body.contains("__TITLE__"));
        assert!(!body.contains("__CONFIG__"));
        assert!(body.contains("\"metricsPath\":\"/metrics\""));
    }

    #[test]
    fn the_script_nonce_matches_the_content_security_policy() {
        // A page whose nonce does not match its policy renders as a blank
        // shell, which looks exactly like a broken proxy.
        let response = render(&enabled_config(), &SecureRandom::new());
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let nonce = csp
            .split("script-src 'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("the policy must carry a script nonce");
        let body = String::from_utf8(body_bytes(&response)).unwrap();
        assert!(body.contains(&format!("<script nonce=\"{nonce}\">")));
        assert!(body.contains(&format!("<style nonce=\"{nonce}\">")));
    }

    #[test]
    fn two_responses_never_share_a_nonce() {
        let rng = SecureRandom::new();
        let config = enabled_config();
        let first = render(&config, &rng);
        let second = render(&config, &rng);
        assert_ne!(
            first.headers().get("content-security-policy"),
            second.headers().get("content-security-policy")
        );
    }

    #[test]
    fn an_operator_supplied_title_cannot_close_the_document() {
        let mut config = enabled_config();
        config.fork.prometheus.title = "</title><script>alert(1)</script>".to_string();
        let response = render(&config, &SecureRandom::new());
        let body = String::from_utf8(body_bytes(&response)).unwrap();
        assert!(!body.contains("<script>alert(1)</script>"));
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
}
