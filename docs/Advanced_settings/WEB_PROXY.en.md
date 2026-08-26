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
else with the site's 404. Case and any port are normalised away, but a trailing
dot is not — `proxy.example.com.` is a distinct name and is refused — and a
request that carries more than one `Host` header is refused before the value is
read at all. A CDN that forwards its own origin hostname also fails the check.
Two ways out:

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

**Use `carrier_mode = "https"`, or `https-lanes` if the public origin speaks
HTTP/2.**

The relay implements four carrier modes — `https`, `https-lanes`, `websocket`,
`websocket-lanes` — because the reference relay does. The carrier itself lives
entirely in the bridge page's JavaScript, which telemt serves byte-identical to
the reference's for all four modes, so a client needs no new transport code for
any of them; telemt's own tests drive a real MTProto handshake through an
`https-lanes` lane.

The two WebSocket carriers are the exception: they are implemented and tested
here, but **not yet verified against a shipping client** (as of 2026-08).
Telegram Desktop's
[WEB proxy plan](https://github.com/telegramdesktop/tdesktop/blob/dev/docs/web-proxy-plan.md)
states that "the v1 HTTPS long-poll carrier is operational; the deployed bridge
does not require a public WebSocket or another carrier", and the transport
"allows only the exact canonical HTTPS bridge navigation": nothing in a shipped
client exercises a WebSocket carrier today, so nothing has proved that its
WebView allows the upgrade. There is no carrier negotiation in v1 either, so a
client cannot report that it does not speak the mode an operator selected.

A mode a client cannot drive fails in a way that looks like nothing is wrong:
the capability resolves, the bridge page renders with the right mode, the
session is created, `sessions_created_total` and `streams_opened_total` both
rise — and the client sits on "connecting" indefinitely, because its 10-second
bridge-message and 30-second write-progress deadlines fail the carrier and
restart it forever. telemt emits a start-up warning naming the profiles that
select a WebSocket carrier.

Treat the two WebSocket modes as unreleased, and retire this note once a
released client has been observed driving one.

```toml
[web]
enabled = true
hostname = "proxy.example.com"      # lowercase ASCII/IDNA, must match the certificate
listen = "127.0.0.1:8080"           # carrier listener, behind the TLS front proxy
admin_listen = "127.0.0.1:8081"     # /healthz, /readyz, /metrics; "" disables
public_dir = "site"                 # operator-owned static site (needs index.html)
# public_upstream = "http://127.0.0.1:3000"   # or a private site application
carrier_mode = "https"              # https or https-lanes: see "Carrier modes"
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
become usable one refresh interval after a configuration reload, without a
restart; see *Operational notes* for what that means for a revoked secret.

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
carrier_mode = "https"                         # see "Carrier modes"

[web.profiles.limits]
max_sessions = 32
max_streams = 512
```

## Carrier modes

**Use `carrier_mode = "https"`, or `https-lanes` if the public origin speaks
HTTP/2.**

The relay implements four carrier modes — `https`, `https-lanes`, `websocket`,
`websocket-lanes` — because the reference relay does. The carrier itself lives
entirely in the bridge page's JavaScript, which telemt serves byte-identical to
the reference's for all four modes, so a client needs no new transport code for
any of them; telemt's own tests drive a real MTProto handshake through an
`https-lanes` lane.

The two WebSocket carriers are the exception: they are implemented and tested
here, but **not yet verified against a shipping client** (as of 2026-08).
Telegram Desktop's
[WEB proxy plan](https://github.com/telegramdesktop/tdesktop/blob/dev/docs/web-proxy-plan.md)
states that "the v1 HTTPS long-poll carrier is operational; the deployed bridge
does not require a public WebSocket or another carrier", and the transport
"allows only the exact canonical HTTPS bridge navigation": nothing in a shipped
client exercises a WebSocket carrier today, so nothing has proved that its
WebView allows the upgrade. There is no carrier negotiation in v1 either, so a
client cannot report that it does not speak the mode an operator selected.

A mode a client cannot drive fails in a way that looks like nothing is wrong:
the capability resolves, the bridge page renders with the right mode, the
session is created, `sessions_created_total` and `streams_opened_total` both
rise — and the client sits on "connecting" indefinitely, because its 10-second
bridge-message and 30-second write-progress deadlines fail the carrier and
restart it forever. telemt emits a start-up warning naming the profiles that
select a WebSocket carrier.

Treat the two WebSocket modes as unreleased, and retire this note once a
released client has been observed driving one.

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
and `[web.timeouts]`. The `[web.limits]` keys carry the reference's names and
defaults unchanged, except `max_carrier_connections`, which the reference does
not have; the `[web.timeouts]` keys do not, and are mapped below the tables.

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
| `max_carrier_connections` | `max_streams_global + 1024` (`5120`) | Carrier connections served at once; must not be smaller than `max_streams_global`, and past it a connection is answered `503` with `Retry-After: 1` |
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

The reference spells its timeouts as Go durations (`"25s"`); telemt spells them
as integer milliseconds, so every shared key gains an `_ms` suffix. The values
are the same:

| Reference `timeouts` key | telemt `[web.timeouts]` key | Default |
| --- | --- | --- |
| `backend_dial` | `backend_dial_ms` | `5000` |
| `long_poll` | `long_poll_ms` | `25000` |
| `reconnect_grace` | `reconnect_grace_ms` | `120000` |
| `bootstrap_lifetime` | `bootstrap_lifetime_ms` | `120000` |
| `read_header` | `read_header_ms` | `10000` |
| `idle` | `idle_ms` | `75000` |
| `shutdown` | — | not implemented |
| — | `body_read_ms` | `30000` |

`body_read_ms` is an addition to the configuration, not to the behaviour: the
reference applies the same 30-second body-read deadline from a constant
(`bodyReadDeadline` in `internal/server/server.go`) rather than exposing it.

There is deliberately no `shutdown` timeout. Telemt closes the WEB carrier
before it drains proxy sessions — `web::shutdown()` runs ahead of
`stop_sessions()` — so a demultiplexed stream ends on the process's own drain
deadline, exactly like a direct client's session. A `shutdown` key would give
one drain two competing deadlines, which is why it was not ported.

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
`reconnect_grace_ms`, so a flapping network cannot lock a client out of its own
slots. That window is the same one the idle reaper uses and is deliberately
wider than the long-poll period: a WebSocket carrier is kept alive by protocol
ping/pong rather than by a poll, so a healthy but quiet session is never
displaced.

`[web.limits]` and `[web.timeouts]` are read once, at start-up. The process-wide
pending pools, the per-session budget partitions, and the accept loops are all
built from them, so a reload cannot change them in place. Reloading a
configuration whose ceilings differ logs a warning and keeps the running values.
The capability profiles reload; see *Operational notes* for exactly which keys
do and which need a restart.

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
`bytes_down_total`, `bridge_pages_served_total`, `stream_bytes_up_total`,
`stream_bytes_down_total`, `limit_hits_total`,
`carrier_connections_dropped_total`, `request_timeouts_total`, and
`retry_later_responses_total`.

`carrier_connections_dropped_total`, `request_timeouts_total`, and
`retry_later_responses_total` are the ones worth alerting on: every other
failure is answered with the site's ordinary 404 by design, so they are the only
externally visible signal that the relay is refusing work.
`carrier_connections_dropped_total` rises when the accept-loop budget is full,
`request_timeouts_total` when a request overran the relay's own deadline, and
`retry_later_responses_total` when a queue budget or a capacity ceiling handed a
client a 503. Protocol, authentication, and budget refusals are logged at
`debug` level with the session id and profile name.

### Diagnosing a carrier that looks healthy

Three counters have no reference counterpart. They exist because the failures
this guide warns about — a mode no WEB client can speak, a secret in a form the
client will not accept, a front proxy the WebView cannot get through — all leave
the relay reporting a perfectly healthy carrier.

- `bridge_pages_served_total` counts pages rendered for a matching capability.
  Compare it with `sessions_created_total`. Pages served with no sessions
  created means clients resolved the capability and loaded the bridge, then
  never reached the carrier: TLS, the front proxy, or the navigation itself.
  Neither counter moving means no client is presenting a capability at all —
  a wrong hostname, a wrong secret, or a stale link.
- `stream_bytes_up_total` and `stream_bytes_down_total` count MTProto payload
  crossing the backend boundary, where `bytes_up_total` and `bytes_down_total`
  count carrier bodies and therefore keep rising for framing, WINDOW grants,
  and empty polls alone. Sessions and streams climbing while the payload
  counters stay near zero is the signature of streams that are being refused:
  check `general.modes` (a WEB client needs `classic` or `secure`; `tls` does
  not help) and `telemt_connections_bad_by_class_total`, whose
  `direct_modes_disabled` and `direct_mtproto_bad_client` classes name the
  reason — though that series also counts direct TCP clients, so read it as
  corroboration rather than as a WEB-only signal.

Because the session bearer travels in a request header on the WebSocket
upgrade, never enable header logging on the front proxy or on telemt.

## Operational notes

- Under `[web]`, only the capability profiles reload: `[access.users]`,
  `[[web.profiles]]`, and the keys that shape them — `derive_user_profiles`,
  `carrier_mode`, and each profile's `backend` and `[web.profiles.limits]`. A
  profile that loses the capability it was created from, because its secret was
  rotated or the profile was deleted, loses its live sessions with it.
- The relay re-derives the profile set on a periodic refresh rather than as a
  step of the reload itself, so revocation is eventual, not immediate. Restart
  telemt when a leaked secret has to stop relaying at a known moment.
- Every other `[web]` key is read once at start-up and needs a restart:
  `enabled`, `hostname`, `listen`, `admin_listen`, `public_dir`,
  `public_upstream`, `trusted_proxies`, `[web.limits]`, and `[web.timeouts]`. A
  reload that changes a ceiling or a timeout logs a warning and keeps the
  running values; a reload that sets `enabled = false` or changes `hostname`
  keeps the whole capability set built at start-up, so it does not turn the
  transport off — a restart does.
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
  telemt refuses a non-loopback `listen` outright unless `trusted_proxies`
  names the off-host front proxy that reaches it: with the shipped default
  (`["127.0.0.0/8", "::1/128"]`) the process exits at configuration load. A
  container deployment that must bind `0.0.0.0` has to add the sibling
  container's address or network to `trusted_proxies`; with that in place it
  starts and only warns.

## Deliberate differences from the reference relay

The wire contract is the reference's, and every divergence below is invisible
to a conforming client. They are listed so that an operator reading the
reference's own documentation is not surprised, and so that a later re-sync
against upstream does not "fix" them back.

- **A `websocket-lanes` session may hold twice `max_streams_per_session`
  lanes.** The reference refuses the upgrade at the stream ceiling, and its
  bridge page treats a refused upgrade as fatal for the whole session. Telemt
  accepts the socket and answers the stream's `OPEN` with a `CLOSE` instead —
  a per-stream failure every client already handles, in place of a per-session
  one. The extra headroom is what lets a drained lane be reclaimed without ever
  refusing a lane a live stream still needs.
- **`Host` matching is case-insensitive and port-agnostic.** The reference
  compares bytes against `hostname` or `hostname:443`. Telemt lowercases and
  strips any port, because nginx and Caddy both do — and byte-exact matching
  turns every non-443 origin port or `Host`-rewriting CDN into a host that 404s
  everything, which is far more conspicuous than serving the operator's index.
  The bridge page is always rendered from the configured `hostname`, so an
  unusual but matching `Host` cannot reach it. A trailing dot is *not*
  normalised away — `hostname.` is a distinct name no browser sends here — and a
  duplicated `Host` header is refused outright, because it is a
  request-smuggling primitive.
- **The bridge document carries a variable-length padding comment.** Without
  it, `GET /?bridge=…` returns a globally constant length on every deployment,
  and a passive observer separates a bridge fetch from an index fetch without
  decrypting anything. The filler is hexadecimal, so it can neither close the
  comment nor reopen markup.
- **The downlink long poll is jittered.** An idle carrier otherwise emits an
  identical request and response pair on an exact 25-second period forever. The
  jitter only ever shortens the park, never lengthens it, so every client
  deadline and every documented multiple of `long_poll_ms` remains an upper
  bound that still holds.
- **The per-address ceilings bucket IPv6 by `/64`, not by exact address.** A
  single subscriber is routinely handed a whole `/64`, so exact-address keying
  would make `max_sessions_per_ip` and `max_bootstraps_per_ip` decorative for
  every IPv6 client. Both ceilings are off by default, so this is inert unless
  an operator turns them on.
- **The loopback-MTProxy backend is retained** alongside in-process
  termination. It is the reference's only mode, so keeping it costs nothing at
  re-sync time, and it is the only configuration in which the protocol's
  credit-timing rule can be exercised at all. In-process termination remains
  the default and the recommended mode.
