# Deploying telemt with the WEB proxy transport, from scratch

Takes a clean server to a working proxy that clients reach through an
app-owned WebView carrier over an ordinary-looking HTTPS site. Every command is
meant to be run in order as `root` on the target host.

- **Assumed target**: Debian 12 or Ubuntu 22.04+, x86_64, public IPv4, systemd.
- **Result**: telemt serving MTProto both directly and through the WEB carrier,
  behind Caddy on 443, with the operator's own site on the same hostname.
- **Reference for every option**:
  [`docs/Advanced_settings/WEB_PROXY.en.md`](../../docs/Advanced_settings/WEB_PROXY.en.md)
  and [`docs/Config_params`](../../docs/Config_params).

If telemt is already installed and running, skip to step 5.

## Quick path

[`install-web.sh`](../../install-web.sh) automates steps 2-9 of this runbook on a
clean Debian or Ubuntu host and prints the credentials and the bridge URL at the
end:

```bash
git clone https://gitlab.corp.alterra.host/nuvira-backend/telemt-webp.git /usr/local/src/telemt
cd /usr/local/src/telemt
./install-web.sh --hostname proxy.example.com --site /path/to/your/site
```

It refuses to guess at anything that matters: it will not invent a public site,
will not overwrite an existing configuration, and will not run without a
hostname that is already canonical. Read the rest of this document before
trusting a host to it — everything the script writes is described below, and
steps 0, 1, and 10 onwards are still yours to do.

## 0. Port layout — decide this first

The bridge page must be fetched over real HTTPS with a publicly trusted
certificate, so **the TLS front proxy owns 443**. telemt's own direct MTProto
listener cannot also bind 443 on the same address.

| Port | Bound by | Exposure |
|---:|---|---|
| 80 | Caddy | public — ACME and the HTTPS redirect |
| 443 | Caddy | public — the site, the bridge, the carrier |
| 8443 | telemt | public — direct MTProto clients (pick any free port) |
| 8080 | telemt | loopback only — WEB carrier |
| 8081 | telemt | loopback only — relay health and metrics |

Two ways to keep direct MTProto on 443 as well, if you need it:

- give the host a **second IP** and bind Caddy to one and telemt to the other
  (`[[server.listeners]] ip = "203.0.113.10"`, `bind 203.0.113.11` in Caddy);
- or drop the direct listener entirely and let every client use the WEB
  transport.

This runbook uses one IP with direct MTProto on **8443**.

## 1. DNS and provider firewall

Add an `A` record for the hostname clients will configure, and wait until it
resolves from outside your network:

```bash
dig +short A proxy.example.com
```

Add an `AAAA` record only if the host really has working public IPv6. Do not
put a CDN or an HTTP proxy in front of this hostname.

In the provider's network firewall allow inbound **TCP 22, 80, 443** and the
direct MTProto port (**8443**). Never expose 8080 or 8081.

If the hostname is internationalised, publish it in its ACE (`xn--…`) form:
clients derive the bridge capability from the A-label, and a hand-typed Unicode
host can be normalised differently across client platforms and then derive a
different capability.

## 2. Build the binary

This fork publishes no release artifact, so the stock `install.sh` and the
stock `Dockerfile` — both of which download a published GitHub release — cannot
install it. Build from the repository instead.

**On the server** (needs ~2 GB RAM; add swap first on a 1 GB box):

```bash
apt update && apt install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

git clone https://gitlab.corp.alterra.host/nuvira-backend/telemt-webp.git /usr/local/src/telemt
cd /usr/local/src/telemt
cargo build --release --locked          # release profile uses fat LTO: several minutes
```

The binary lands at `target/release/telemt`.

**Or build elsewhere and copy** — the toolchain must target the server's libc:

```bash
docker build -f contrib/web/Dockerfile.source -t telemt-webp:local .
docker create --name extract telemt-webp:local && \
  docker cp extract:/app/telemt ./telemt && docker rm extract
scp telemt root@YOUR_SERVER:/tmp/telemt
```

## 3. Service account, directories, binary, unit

```bash
groupadd --system telemt 2>/dev/null || true
useradd --system --gid telemt --home-dir /var/lib/telemt \
        --shell /usr/sbin/nologin --no-create-home telemt 2>/dev/null || true

install -d -o telemt -g telemt -m 0700 /var/lib/telemt
install -d -o root   -g telemt -m 0750 /etc/telemt

install -m 0755 /usr/local/src/telemt/target/release/telemt /usr/bin/telemt
telemt --version
```

Install the shipped unit and point it at the config path used below:

```bash
install -m 0644 /usr/local/src/telemt/contrib/systemd/telemt.service \
                /etc/systemd/system/telemt.service
sed -i 's#/etc/telemt/telemt.toml#/etc/telemt/config.toml#' /etc/systemd/system/telemt.service
systemctl daemon-reload
```

The unit already runs as `telemt:telemt` from `/var/lib/telemt` with
`NoNewPrivileges` and only `CAP_NET_BIND_SERVICE`. Nothing about the WEB
transport needs more privilege: both relay listeners are loopback.

## 4. Base configuration

Generate one secret per user — this is the same secret the user pastes into
their app, and the WEB capability is derived from it:

```bash
openssl rand -hex 16
```

Write `/etc/telemt/config.toml`, substituting the hostname and secret:

```toml
[general]
use_middle_proxy = false
log_level = "normal"

[general.modes]
classic = false
# secure carries the WEB transport: a WEB client can only use a plain or dd
# secret, and dd keeps the padded transform.
secure = true
# tls serves direct fake-TLS clients on 8443; WEB clients never use it.
tls = true

[general.links]
show = "*"
public_host = "proxy.example.com"
public_port = 8443

[server]
port = 8443

[[server.listeners]]
ip = "0.0.0.0"
port = 8443

[server.api]
enabled = true
listen = "127.0.0.1:9091"
whitelist = ["127.0.0.1/32", "::1/128"]

[censorship]
tls_domain = "www.google.com"
mask = true

[access.users]
alice = "PASTE_THE_32_HEX_SECRET_HERE"
```

```bash
chown root:telemt /etc/telemt/config.toml && chmod 0640 /etc/telemt/config.toml
```

Set `use_middle_proxy = true` only if you also configure an `ad_tag`; direct
mode needs no Telegram-side registration and no outbound fetch at start-up.

## 5. The operator's public site

The repository deliberately ships no site: if many operators deployed the same
starter page, its body and assets would become an easy active-probing
signature. Use a site that genuinely belongs to you.

```bash
install -d -o telemt -g telemt -m 0755 /var/lib/telemt/site
# copy your generated site in; index.html is required, 404.html strongly recommended
printf '<!doctype html><title>Example</title><h1>Example</h1>' > /var/lib/telemt/site/index.html
printf '<!doctype html><title>Not found</title><h1>Not found</h1>' > /var/lib/telemt/site/404.html
chown -R telemt:telemt /var/lib/telemt/site
```

The site is read into memory once at start-up — **restart telemt after changing
files**. For a CMS, accounts, forms, SSE, or the site's own WebSockets, run
that application on a loopback port and use `public_upstream` instead of
`public_dir`.

## 6. Enable the WEB transport

Append to `/etc/telemt/config.toml`:

```toml
[web]
enabled = true
hostname = "proxy.example.com"
listen = "127.0.0.1:8080"
admin_listen = "127.0.0.1:8081"
public_dir = "/var/lib/telemt/site"
carrier_mode = "https"
derive_user_profiles = true
trusted_proxies = ["127.0.0.0/8", "::1/128"]
```

`derive_user_profiles = true` gives every `[access.users]` entry WEB access
with the secret it already has — no second credential to distribute.

**Use `carrier_mode = "https"`.** The relay implements four carrier modes. The
carrier itself lives in the bridge page's JavaScript, which is byte-identical to
the reference relay's for all four, so a client needs no new transport code for
any of them, and `https-lanes` is driven end to end by telemt's own tests.

The two WebSocket carriers are **not yet verified against a shipping client**
(as of 2026-08). The client's own
[plan](https://github.com/telegramdesktop/tdesktop/blob/dev/docs/web-proxy-plan.md)
states that "the v1 HTTPS long-poll carrier is operational; the deployed bridge
does not require a public WebSocket or another carrier", and the client performs
no carrier negotiation — so it cannot tell you it does not speak the mode you
chose. A mode a client cannot drive produces a deployment where the bridge page
renders, the session is created, every server-side counter looks healthy, and
the client sits on "connecting" forever while its 10-second bridge and 30-second
write-progress deadlines drive an endless reconnect loop. telemt warns at
start-up about a profile that selects a WebSocket carrier.

## 7. Caddy

```bash
apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | tee /etc/apt/sources.list.d/caddy-stable.list
chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg
chmod o+r /etc/apt/sources.list.d/caddy-stable.list
apt update && apt install -y caddy
```

Install the template and substitute the hostname:

```bash
install -m 0644 /usr/local/src/telemt/contrib/web/Caddyfile.example /etc/caddy/Caddyfile
sed -i 's/proxy\.example\.com/YOUR.HOSTNAME/g' /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
```

An existing nginx host can use [`nginx.conf.example`](nginx.conf.example)
instead. Whichever you use, three things matter:

- **every path** is forwarded to telemt — there is no separately hosted relay
  path, so an unauthenticated prober only ever sees your site;
- no read timeout shorter than the long-poll period (25 s by default);
- `X-Forwarded-For` must carry the address the front proxy observed. Caddy does
  this by default and needs no directive; nginx needs `$remote_addr` rather
  than `$proxy_add_x_forwarded_for`. telemt trusts the header only from
  `web.trusted_proxies`, and reads the last entry of a list.

Access logs go to the journal by default. Never enable request-header logging
on this site: the session bearer travels in a header on the WebSocket upgrade.
If you add a `log { output file … }` block, create the directory first
(`mkdir -p /var/log/caddy && chown caddy:caddy /var/log/caddy`) — otherwise
Caddy exits at start-up with `permission denied`.

## 8. Host firewall

```bash
ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw allow 8443/tcp
ufw deny 8080/tcp && ufw deny 8081/tcp
ufw --force enable
```

The provider's network firewall is the second required boundary. The relay
listeners must never be reachable from outside the host.

## 9. Start

```bash
systemctl enable --now telemt
systemctl enable --now caddy
systemctl --no-pager --full status telemt caddy
```

Caddy obtains the certificate on first request to the hostname; watch
`journalctl -u caddy -f` if it does not appear within a minute.

## 10. Verify

On the server:

```bash
journalctl -u telemt -n 30 --no-pager | grep -i "WEB proxy"
# expect: WEB proxy listener bound ... carrier / admin
#         WEB proxy transport enabled hostname=... profiles=N

curl --fail http://127.0.0.1:8081/healthz     # ok
curl --fail http://127.0.0.1:8081/readyz      # ready
curl --silent http://127.0.0.1:8081/metrics | head -5

ss -lntp | grep -E '8080|8081|8443|:443|:80'
```

From your own machine — the site must look completely ordinary:

```bash
curl -sSI https://proxy.example.com/        | head -3    # 200, your index
curl -sS  https://proxy.example.com/nope -o /dev/null -w '%{http_code}\n'   # 404
```

Only a valid capability reveals the bridge. Derive one from a user's secret and
fetch it:

```bash
python3 - <<'PY'
import base64, hmac, hashlib
host, secret_hex = "proxy.example.com", "PASTE_THE_32_HEX_SECRET_HERE"
mac = hmac.new(bytes.fromhex(secret_hex),
               ("tdesktop-web-proxy-bridge-v1\n" + host).encode(), hashlib.sha256)
print(f"https://{host}/?bridge=" + base64.urlsafe_b64encode(mac.digest()).decode().rstrip("="))
PY
```

```bash
curl -sS -D - -o /dev/null "PASTE_THE_PRINTED_URL" | grep -iE 'HTTP/|content-security-policy'
# expect: HTTP/2 200
#         content-security-policy: default-src 'none'; ... script-src 'nonce-...'
```

It must be a **GET**: the bridge is issued only for `GET /`, so `curl -I` sends
`HEAD`, receives the ordinary index, and makes a healthy relay look broken. The
`default-src 'self'` policy of the static site is the tell that you fetched the
index instead of the bridge.

Any other query on `/` must return your ordinary index, not the bridge.

## 11. Give the credentials to a user

A WEB-capable client needs **only two values** and derives the capability
itself; the secret never reaches the page:

- host: `proxy.example.com`
- secret: the user's **`dd`-prefixed** secret, `dd<32-hex>`

A WEB client accepts only a plain or `dd` secret and refuses `ee` fake-TLS
secrets outright, so hand out the `dd…` form and keep `secure = true`. The
plain 32-hex secret works too, but only with `classic = true`. Confirm what the
running config accepts:

```bash
journalctl -u telemt --no-pager | grep -i "secret form"
```

For direct MTProto clients, telemt prints `tg://` links at start-up
(`[general.links] show = "*"`):

```bash
journalctl -u telemt --no-pager | grep -i 'tg://\|t.me/proxy'
```

## 12. Observe

```text
tproxy_sessions_live                 # 127.0.0.1:8081/metrics
tproxy_streams_live
tproxy_streams_rejected_total        # rising: a limit is too tight
tproxy_limit_hits_total
tproxy_backend_dial_failures_total   # only meaningful for loopback backends
tproxy_bridge_pages_served_total     # compare with sessions_created_total
tproxy_stream_bytes_up_total         # MTProto payload, not carrier bodies
tproxy_stream_bytes_down_total
```

The same series appear on telemt's own metrics endpoint prefixed
`telemt_web_`.

The last three answer the two questions a 404-by-design relay otherwise cannot.
`bridge_pages_served_total` far ahead of `sessions_created_total` means clients
load the bridge and never reach the carrier — TLS, the front proxy, or the
navigation. Sessions and streams climbing while the `stream_bytes_*` pair stays
near zero means the carrier is fine and every stream is being refused; the
`bytes_*` pair keeps rising there because it counts framing and empty polls too.

## 13. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to bind WEB proxy listener` at start-up | 8080/8081 already used | change `web.listen` / `web.admin_listen` |
| telemt exits with `No listeners. Exiting.` | no usable `[[server.listeners]]` | check the listener block and that the port is free |
| Caddy fails to start | telemt already holds 443 | see step 0: the front proxy owns 443 |
| Every request returns 404, including `/` | `Host` seen by telemt ≠ `web.hostname` | `journalctl -u telemt \| grep "Host does not match"` names both; make the front proxy preserve `Host`, or rewrite it with `header_up Host <hostname>` |
| Every request returns 404 behind a CDN | the CDN forwards its own origin hostname, and `web.hostname` must be the name clients type | set `web.hostname` to the client-facing name and normalise `Host` at the origin proxy |
| Bridge URL returns the ordinary index | wrong secret, wrong hostname, or non-canonical `?bridge=` | re-derive with the exact hostname and the exact secret string the client uses |
| Client connects, carrier looks healthy, no data ever flows | no mode a WEB client can speak is enabled | confirm with `tproxy_stream_bytes_down_total` flat while `tproxy_sessions_created_total` climbs; set `secure = true` and hand out the `dd…` secret. The reject shows as `direct_modes_disabled` in the bad-connect classes |
| Bridge page is served but no session is ever created | the WebView loaded the page and could not reach the carrier | `tproxy_bridge_pages_served_total` rising with `tproxy_sessions_created_total` flat: check that the front proxy forwards `/api/v1/*` to telemt and does not buffer or rewrite it |
| Client stays on "connecting" forever; sessions are created and counters look healthy; it connects instantly after switching to another proxy and back | `carrier_mode` selects a WebSocket carrier the client's WebView does not drive | set `carrier_mode = "https"` and restart |
| Client rejects the secret in its proxy settings | an `ee` fake-TLS secret was handed out | WEB clients accept only plain and `dd` secrets |
| `502 Bad Gateway` | telemt down or wrong upstream port | `systemctl status telemt`, check `web.listen` |
| Caddy exits with `permission denied` on a log file | `/var/log/caddy` missing or not writable by `caddy` | drop the `log` block, or `mkdir -p /var/log/caddy && chown caddy:caddy /var/log/caddy` |
| Caddy warns `Unnecessary header_up X-Forwarded-For` | Caddy already sets the header | remove the directive |
| Client address logged as the CDN, or requests rejected | a CDN fronts Caddy | do not front this hostname with a CDN: long polls and WebSocket carriers need unbuffered pass-through |
| Long poll cut after a few seconds | front-proxy read timeout below 25 s | raise it or disable it for this site |
| `/readyz` returns 503 | loopback backend unreachable, or admission closed | check the profile `backend`, then `journalctl -u telemt` |
| Unknown config key rejected at start-up | `general.config_strict = true` and a typo | the error names the key and suggests the nearest valid one |

Raise verbosity temporarily with `systemctl edit telemt` and
`Environment=RUST_LOG=telemt::web=debug`.

## 14. Updating

```bash
cd /usr/local/src/telemt && git pull && cargo build --release --locked
install -m 0755 target/release/telemt /usr/bin/telemt
systemctl restart telemt
```

Configuration-only changes are picked up by the config watcher, but under
`[web]` only the capability profiles reload: `[access.users]`,
`[[web.profiles]]`, and the keys that shape them (`derive_user_profiles`,
`carrier_mode`, each profile's `backend` and `[web.profiles.limits]`). Adding a
user therefore needs no restart, and a rotated or deleted secret loses the
sessions it opened.

The relay re-derives that set on a periodic refresh rather than as a step of the
reload, so revocation is eventual, not immediate — restart telemt when a leaked
secret has to stop relaying at a known moment.

Every other `[web]` key is read once at start-up and needs a restart:
`enabled`, `hostname`, `listen`, `admin_listen`, `public_dir`,
`public_upstream`, `trusted_proxies`, `[web.limits]`, and `[web.timeouts]`.
That includes `enabled`: a reload cannot turn the transport off, which is why
step 15 restarts.

## 15. Rollback and uninstall

Turn the transport off without touching anything else — it is additive, and
telemt behaves exactly as before when it is disabled:

```bash
sed -i 's/^enabled = true/enabled = false/' /etc/telemt/config.toml   # inside [web]
systemctl restart telemt
```

Remove it entirely:

```bash
systemctl disable --now telemt caddy
rm -f /etc/systemd/system/telemt.service /usr/bin/telemt
rm -rf /etc/telemt /var/lib/telemt
systemctl daemon-reload
userdel telemt && groupdel telemt
```

## Appendix: deliberate differences from the reference relay

telemt implements the reference relay's wire contract, and none of these is
visible to a conforming client. They are listed because they will surprise
anyone reading the reference's own documentation alongside this runbook.

| Difference | Why |
|---|---|
| A `websocket-lanes` session may hold twice `max_streams_per_session` lanes, and a lane past the stream ceiling gets a per-stream `CLOSE` instead of a refused upgrade | the reference's refused upgrade is fatal for the whole bridge; a `CLOSE` is a failure every client already handles |
| `Host` matching ignores case, a trailing dot, and any port | the reference is byte-exact on `hostname` or `hostname:443`, which 404s everything behind a non-443 origin port or a `Host`-rewriting CDN |
| The bridge page carries a variable-length padding comment | without it `GET /` is the same length on every deployment, so its `Content-Length` alone separates a bridge fetch from an index fetch |
| The downlink long poll is jittered | an idle carrier otherwise polls on an exact 25 s period forever; the jitter only ever shortens the park, so no client deadline moves |
| Per-address ceilings count an IPv6 client per `/64` | a subscriber is routinely handed a whole `/64`, so exact-address keying would make those ceilings decorative; both are off by default |
| The loopback-MTProxy backend is kept alongside in-process termination | it is the reference's only mode, so keeping it costs nothing; in-process stays the default and the recommended mode |

Full reasoning is in
[`WEB_PROXY.en.md`](../../docs/Advanced_settings/WEB_PROXY.en.md#deliberate-differences-from-the-reference-relay).
