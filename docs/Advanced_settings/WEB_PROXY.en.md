# WEB proxy transport

The WEB proxy transport lets a WEB-capable Telegram app reach telemt through a
carrier that looks like an ordinary HTTPS website. The app keeps its normal
MTProxy framing and encryption, but sends every proxy connection through one
app-owned WebView transport instead of raw TCP.

It implements the client-independent wire contract of
[`telegramdesktop/tproxy-server`](https://github.com/telegramdesktop/tproxy-server)
(WEB proxy protocol v1), so any client that speaks that protocol — the Telegram
Desktop proof of concept, the Android WebView bridge, or the iOS client — works
against telemt unchanged.

## How it differs from the reference relay

The reference relay hands every demultiplexed stream to a stock MTProxy over a
loopback TCP connection. Telemt terminates the stream **in-process**: the bytes
go straight into the same client pipeline a direct TCP client would use, so a
WEB client gets fake-TLS handling, Middle-End routing, per-user limits, quotas,
IP tracking, statistics, and masking exactly like every other client — with two
fewer syscalls and two fewer kernel copies per chunk.

The loopback backend is still available (`backend = "127.0.0.1:2398"`) for
deployments that really want a separate MTProxy process behind the carrier.

```text
Internet :443 -> TLS front proxy (Caddy/nginx) -> 127.0.0.1:8080 telemt WEB carrier
                                                     |
                                                     +-> in-process MTProto client pipeline
                                                     +-> operator site (memory or loopback app)
```

## Requirements

The bridge page must be fetched over real HTTPS with a publicly trusted
certificate for the configured hostname, so a TLS-terminating front proxy is
required. Only the front proxy listens on a public interface; the carrier and
admin listeners stay on loopback.

A minimal Caddy configuration:

```caddy
proxy.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy must proxy **every** path to telemt. There is no separately hosted relay
path for an unauthenticated prober to compare with the public site: only
`GET /?bridge=<valid capability>` reveals the bridge, and every other request
is answered by the operator's own site.

### Behind a CDN or a second proxy layer

`web.hostname` must be **the hostname clients type into their app**, because the
bridge capability is `HMAC(secret, "…\n" + hostname)` — the relay and the client
must derive it over the same name. It is not the origin's own hostname.

telemt then checks the `Host` header against that name and answers everything
else with the site's 404. Case, a trailing dot, and any port are normalised
away, but a CDN that forwards its own origin hostname still fails the check. Two
ways out:

- point clients and the CDN at the same name (`hostname = "cdn.example.com"`)
  and let the CDN preserve `Host`; or
- normalise it at the origin front proxy, which is the reliable option when the
  CDN insists on rewriting:

```caddy
cdn.example.com, origin.example.com {
	reverse_proxy 127.0.0.1:8080 {
		header_up Host cdn.example.com
	}
}
```

A rejected request logs the received and expected names at debug level:

```bash
journalctl -u telemt | grep "Host does not match"
```

Two further caveats apply behind a CDN: it terminates TLS, so it sees the bridge
page, the bearer tokens, and the carrier's request pattern in the clear; and
`X-Forwarded-For` then ends with the CDN's edge address, so per-IP limits and IP
tracking account every user to that address.

## Configuration

## Carrier modes

**Use `carrier_mode = "https"`.**

The relay implements four carrier modes — `https`, `https-lanes`, `websocket`,
`websocket-lanes` — because the reference relay does. The current client
implements exactly one of them: the HTTPS long-poll carried by its hidden
WebView. Telegram Desktop's
[WEB proxy plan](https://github.com/telegramdesktop/tdesktop/blob/dev/docs/web-proxy-plan.md)
states that "the v1 HTTPS long-poll carrier is operational; the deployed bridge
does not require a public WebSocket or another carrier", and the transport
"allows only the exact canonical HTTPS bridge navigation". There is no carrier
negotiation in v1, so a client cannot report that it does not speak the mode an
operator selected.

Choosing any other mode produces a failure that looks like nothing is wrong:
the capability resolves, the bridge page renders with the right mode, the
session is created, `sessions_created_total` and `streams_opened_total` both
rise — and the client sits on "connecting" indefinitely, because its 10-second
bridge-message and 30-second write-progress deadlines fail the carrier and
restart it forever. telemt emits a start-up warning naming the affected
profiles.

The other three modes remain in the relay for a client that implements them.
Treat them as unreleased.

```toml
[web]
enabled = true
hostname = "proxy.example.com"      # lowercase ASCII/IDNA, must match the certificate
listen = "127.0.0.1:8080"           # carrier listener, behind the TLS front proxy
admin_listen = "127.0.0.1:8081"     # /healthz, /readyz, /metrics; "" disables
public_dir = "site"                 # operator-owned static site (needs index.html)
# public_upstream = "http://127.0.0.1:3000"   # or a private site application
carrier_mode = "https"              # keep this: see "Carrier modes" below
derive_user_profiles = true         # every [access.users] entry gets WEB access
trusted_proxies = ["127.0.0.0/8", "::1/128"]
```

Exactly one of `public_dir` and `public_upstream` is required. In directory
mode the whole site is read into memory once at start-up, and every path — the
index, assets, and the 404 body — is served by one code path with one header
set. Restart telemt after changing the files. In application mode ordinary and
unauthenticated requests are delegated to one private loopback web application,
which owns its framework, headers, cookies, and persistence; the four transport
paths remain reserved.

### Client setup

Clients configure a hostname and an MTProxy secret, nothing else. The bridge
capability is derived locally:

```text
capability = base64url-no-padding(HMAC-SHA256(secret, "tdesktop-web-proxy-bridge-v1\n" + hostname))
URL        = https://<hostname>/?bridge=<capability>
```

With `derive_user_profiles = true` every user in `[access.users]` can use the
WEB transport with the secret they already have, in either form a WEB client
accepts: the plain 32-hex secret or its `dd` random-padding form. New users
become usable within one refresh interval (30 s) of a configuration reload; no
restart is needed.

**`ee` fake-TLS secrets do not work over this transport**, and a WEB-capable
client refuses to accept one in its proxy settings. The carrier is a raw relay:
it does not add the inner TLS-emulation record that an `ee` secret implies.

**So enable a mode a WEB client can actually speak.** The bridge capability and
the MTProto handshake are separate checks: the capability gets the client to the
bridge, but the stream it opens then speaks the plain (classic) or `dd` (secure)
transform. If neither `[general.modes] classic` nor `secure` is enabled, every
stream is refused and masked while the carrier itself stays healthy — sessions
are created, streams open, and no data flows. `general.modes.tls` does not help
here; it governs the fake-TLS transform that WEB clients never offer.

A deployment serving both direct and WEB clients typically enables `secure` for
WEB (hand out the `dd…` secret) and `tls` for direct clients (hand out the
`ee…` link). telemt logs the accepted forms at start-up and warns when no mode
a WEB client can use is enabled.

### Explicit profiles

Explicit profiles bind one secret to a backend, a carrier mode, and its own
ceilings. An explicit profile takes precedence over a derived user profile of
the same name.

```toml
[[web.profiles]]
name = "media"
secret = "000102030405060708090a0b0c0d0e0f"   # hex or base64url
backend = "internal"                           # or "127.0.0.1:2398"
carrier_mode = "https"                         # see "Carrier modes" below

[web.profiles.limits]
max_sessions = 32
max_streams = 512
```

## Carrier modes

| Mode | Shape | Trade-off |
| --- | --- | --- |
| `https` | One serialized uplink and one long-poll downlink | Simplest; a busy direction is bounded by roughly `carrier_batch_bytes / RTT` |
| `https-lanes` | One independent request lane per logical stream | Removes carrier head-of-line blocking; needs HTTP/2 at the public origin |
| `websocket` | One multiplexed WebSocket | Removes the stop-and-wait ceiling; no per-stream isolation |
| `websocket-lanes` | One WebSocket per logical stream | Best isolation; more sockets and TLS setup per session |

The mode is fixed for a session at creation and is baked into the bridge page,
so clients need no new setting when an operator changes it.

## Limits and timeouts

Every ceiling from the reference relay is configurable under `[web.limits]`
and `[web.timeouts]`, with the same names and defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `max_header_bytes` | `16384` | Largest accepted request header block |
| `max_body_bytes` | `2097152` | Largest accepted carrier request body |
| `max_frame_payload` | `1048576` | Largest accepted single frame payload |
| `carrier_batch_bytes` | `2097152` | Target size of one downlink batch (2 MiB cap) |
| `max_streams_per_session` | `128` | Live streams per session |
| `max_closed_stream_ids` | `4096` | Stream-id tombstones kept for close races |
| `max_pending_per_session` | `33554432` | Queued bytes per session |
| `max_pending_global` | `536870912` | Queued bytes across the process |
| `max_pending_items_per_session` | `16384` | Queued items per session |
| `max_pending_items_global` | `262144` | Queued items across the process |
| `max_sessions_per_ip` | `0` (off) | Sessions one client address may hold |
| `max_sessions_global` | `128` | Live sessions |
| `max_streams_global` | `4096` | Live streams |
| `max_backend_dials_in_flight` | `256` | Backend connections establishing at once |
| `new_sessions_per_minute` / `new_sessions_burst` | `600` / `128` | Session creation rate |
| `new_streams_per_minute` / `new_streams_burst` | `6000` / `512` | Stream creation rate |
| `max_bootstraps_per_ip` | `0` (off) | Unconsumed bootstraps per address |
| `max_bootstraps_global` | `512` | Unconsumed bootstraps |
| `new_bootstraps_per_minute` / `new_bootstraps_burst` | `1200` / `256` | Bootstrap issuance rate |
| `max_profiles` | `32` | Explicit profile entries |

| Timeout | Default | Meaning |
| --- | --- | --- |
| `backend_dial_ms` | `5000` | Loopback backend dial deadline |
| `long_poll_ms` | `25000` | Long-poll parking period and WebSocket ping cadence |
| `reconnect_grace_ms` | `120000` | Idle period after which a session is reaped |
| `bootstrap_lifetime_ms` | `120000` | Bootstrap token lifetime |
| `read_header_ms` | `10000` | Request-head deadline |
| `body_read_ms` | `30000` | Request-body deadline |
| `idle_ms` | `75000` | Keep-alive idle period |

Per-profile overrides live under `[web.profiles.limits]` and may only lower the
process-wide ceiling they refine.

Both per-address ceilings are **off by default**, and turning them on is a
decision to make deliberately. They count an IPv6 client per `/64`, not per
address — a single subscriber is routinely handed a whole `/64` — and they apply
to the address telemt resolves through `trusted_proxies`, so behind a front
proxy they count the real client rather than the proxy.

The reason they default to off is carrier-grade NAT. Most clients of a
circumvention proxy share one public address with thousands of strangers, and
`max_sessions_per_ip` counts *live* sessions, which includes every session a
client abandoned when its network dropped: those stay live for the whole
`reconnect_grace_ms`. Any value low enough to bound one attacker is low enough
to lock out a whole mobile carrier, and the session creation rate limits
(`new_sessions_per_minute` / `new_sessions_burst`) bound the same abuse without
that side effect.

`max_bootstraps_per_ip` fails worse still: a refused bootstrap cannot be
reported, because an error would confirm to a prober that the capability was
valid, so the client is served the ordinary index and fails with no retry and no
diagnostic.

Enable them only when clients have addresses of their own. When
`max_sessions_per_ip` is set, a client reconnecting from an address at its
ceiling first reclaims its own sessions that have been silent for more than
twice `long_poll_ms`, so a flapping network cannot lock a client out of its own
slots; a session still driving its carrier is never displaced.

`[web.limits]` and `[web.timeouts]` are read once, at start-up. The process-wide
pending pools, the per-session budget partitions, and the accept loops are all
built from them, so a reload cannot change them in place. Reloading a
configuration whose ceilings differ logs a warning and keeps the running values;
`[access.users]` and `[[web.profiles]]` still reload normally. Rotating a
profile's secret closes the live sessions that secret opened.

## Observability

The admin listener keeps the reference paths and metric names:

```bash
curl --fail http://127.0.0.1:8081/healthz
curl --fail http://127.0.0.1:8081/readyz
curl --silent http://127.0.0.1:8081/metrics
```

`/readyz` checks that every profile can accept a stream: loopback backends are
dialled, and internal backends require open runtime admission.

The same counters appear on telemt's main metrics endpoint under the
`telemt_web_` prefix: `sessions_live`, `streams_live`,
`backend_dials_in_flight`, `pending_bytes`, `pending_items`,
`sessions_created_total`, `sessions_closed_total`, `streams_opened_total`,
`streams_rejected_total`, `backend_dial_failures_total`, `bytes_up_total`,
`bytes_down_total`, `limit_hits_total`, `carrier_connections_dropped_total`,
`request_timeouts_total`, and `retry_later_responses_total`.

The last three are the ones worth alerting on: every other failure is answered
with the site's ordinary 404 by design, so they are the only externally visible
signal that the relay is refusing work.
`carrier_connections_dropped_total` rises when the accept-loop budget is full,
`request_timeouts_total` when a request overran the relay's own deadline, and
`retry_later_responses_total` when a queue budget or a capacity ceiling handed a
client a 503. Protocol, authentication, and budget refusals are logged at
`debug` level with the session id and profile name.

Because the session bearer travels in a request header on the WebSocket
upgrade, never enable header logging on the front proxy or on telemt.

## Operational notes

- `hostname`, `listen`, `admin_listen`, `public_dir`, `public_upstream`,
  `[web.limits]`, and `[web.timeouts]` are read once at start-up; changing them
  requires a restart, and a reload that changes them logs a warning and keeps
  the running values. Capability profiles (`[access.users]` and
  `[[web.profiles]]`) are re-derived after a reload, and a profile whose secret
  changed loses its live sessions with it.
- The `websocket` carrier ties the session to its socket. The `https` carriers
  survive a dropped request because each uplink carries its sequence and each
  downlink its cursor, so `reconnect_grace_ms` covers them; the v1 WebSocket
  subprotocol carries only the bearer, so a replacement socket has no way to
  state where it left off and the session ends with the socket. Prefer an
  `https` mode where middleboxes cut long-lived connections.
- Each demultiplexed stream is a normal telemt session: it consumes a slot from
  `server.max_connections` and is drained by the usual shutdown sequence.
- `X-Forwarded-For` is honoured only from `trusted_proxies`. A request from an
  untrusted source is accounted against its own peer address and its forwarding
  header is ignored. When the header carries a list, the last entry is used:
  every proxy appends its own observation, so that entry is the one a client
  cannot inject.
- Do not expose `listen` or `admin_listen` on a public interface. The carrier
  listener is plaintext HTTP by design — TLS belongs to the front proxy — so
  anything that can reach it reads the bridge capability and the session bearer.
  telemt warns at start-up when `listen` is not a loopback address; it is not
  refused only because a container deployment reaches the relay from a sibling
  container.
