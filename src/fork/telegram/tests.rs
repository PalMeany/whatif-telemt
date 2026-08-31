//! The bot's Bot API calls, driven against a local server.
//!
//! `api_base` accepts an `http://` origin so a self-hosted Bot API server can
//! be used; that is what makes this testable without TLS or a real token. What
//! is covered here is everything the bot puts on the wire and everything it
//! believes about the reply — the parts a unit test of `split_message` or
//! `parse` cannot reach.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::client::BotClient;
use crate::config::ForkTelegramConfig;

/// One request the mock server saw.
#[derive(Clone, Debug)]
struct Seen {
    /// Request target, which carries the token and the method name.
    path: String,
    /// Request body, verbatim.
    body: String,
}

/// A Bot API stand-in that answers a fixed body and records what it was asked.
struct MockApi {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    served: Arc<AtomicUsize>,
}

impl MockApi {
    /// Starts a server that answers `status` with `body` for every request.
    async fn start(status: u16, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port must be available");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));

        let recorded = seen.clone();
        let counter = served.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // The client sends `Connection: close` and a content-length
                    // body, so reading until the body is complete is enough.
                    loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(entry) = parse_request(&buffer) {
                            recorded.lock().push(entry);
                            break;
                        }
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        Self {
            origin: format!("http://{addr}"),
            seen,
            served,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::Relaxed)
    }
}

/// Splits a complete request into its target and body, or nothing while it is
/// still arriving.
fn parse_request(buffer: &[u8]) -> Option<Seen> {
    let text = std::str::from_utf8(buffer).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if body.len() < length {
        return None;
    }
    Some(Seen {
        path,
        body: body.to_string(),
    })
}

fn client_for(origin: &str) -> BotClient {
    BotClient::new(&ForkTelegramConfig {
        enabled: true,
        token: "123456:AAHtesttokenvalue".to_string(),
        admins: vec![1],
        api_base: origin.to_string(),
        poll_timeout_secs: 1,
        request_timeout_secs: 5,
        ..ForkTelegramConfig::default()
    })
}

/// A `getUpdates` reply carrying one message and one update the bot ignores.
const UPDATES: &str = r#"{"ok":true,"result":[
  {"update_id":41,"message":{"chat":{"id":7},"from":{"id":9},"text":"/status"}},
  {"update_id":42,"edited_message":{"chat":{"id":7},"text":"ignored"}}
]}"#;

#[tokio::test]
async fn polling_returns_text_messages_and_advances_the_offset() {
    let api = MockApi::start(200, UPDATES).await;
    let client = client_for(&api.origin);

    let (commands, offset) = client.poll(0, None).await.expect("the mock must answer");

    assert_eq!(commands.len(), 1, "only text messages become commands");
    assert_eq!(commands[0].chat_id, 7);
    assert_eq!(commands[0].from_id, 9);
    assert_eq!(commands[0].text, "/status");
    assert_eq!(
        offset, 43,
        "the offset must clear every update in the batch, not just the ones answered"
    );
}

#[tokio::test]
async fn a_poll_puts_the_token_in_the_path_and_the_offset_in_the_body() {
    let api = MockApi::start(200, UPDATES).await;
    let client = client_for(&api.origin);

    client.poll(17, None).await.expect("the mock must answer");

    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/bot123456:AAHtesttokenvalue/getUpdates");
    let body: serde_json::Value =
        serde_json::from_str(&requests[0].body).expect("the request body must be JSON");
    assert_eq!(body["offset"], 17);
    assert_eq!(
        body["allowed_updates"],
        serde_json::json!(["message"]),
        "asking for less keeps the token from reading unrelated chats"
    );
}

#[tokio::test]
async fn a_non_200_reply_is_an_error_rather_than_an_empty_batch() {
    // A silently empty batch would look exactly like an idle chat, and the bot
    // would poll a broken endpoint for ever without saying so.
    let api = MockApi::start(401, r#"{"ok":false,"description":"Unauthorized"}"#).await;
    let client = client_for(&api.origin);

    let error = client
        .poll(0, None)
        .await
        .expect_err("an HTTP failure must surface");

    assert!(error.to_string().contains("401"), "unexpected: {error}");
}

#[tokio::test]
async fn a_refusal_inside_a_200_reply_is_still_an_error() {
    let api = MockApi::start(200, r#"{"ok":false,"description":"terminated by other"}"#).await;
    let client = client_for(&api.origin);

    let error = client
        .poll(0, None)
        .await
        .expect_err("an API-level refusal must surface");

    assert!(
        error.to_string().contains("terminated by other"),
        "the reason Telegram gave must reach the log: {error}"
    );
}

#[tokio::test]
async fn a_reply_is_posted_to_the_chat_it_answers() {
    let api = MockApi::start(200, r#"{"ok":true,"result":{}}"#).await;
    let client = client_for(&api.origin);

    client
        .send(7, "two\nlines", None)
        .await
        .expect("the mock must accept the reply");

    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/bot123456:AAHtesttokenvalue/sendMessage");
    let body: serde_json::Value =
        serde_json::from_str(&requests[0].body).expect("the request body must be JSON");
    assert_eq!(body["chat_id"], 7);
    assert_eq!(body["text"], "two\nlines");
    assert!(
        body.get("parse_mode").is_none(),
        "replies are plain text: a secret must never be reinterpreted as markup"
    );
}

#[tokio::test]
async fn an_oversized_reply_is_sent_as_several_messages() {
    let api = MockApi::start(200, r#"{"ok":true,"result":{}}"#).await;
    let client = client_for(&api.origin);
    let long = std::iter::repeat_n("x".repeat(100), 80)
        .collect::<Vec<_>>()
        .join("\n");

    client
        .send(7, &long, None)
        .await
        .expect("the mock must accept every chunk");

    assert!(
        api.served() > 1,
        "a reply past the Bot API limit must be split rather than refused"
    );
}
