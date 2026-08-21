# Deploying the WEB proxy transport

Checklist for putting a telemt build with `[web]` enabled behind a TLS front
proxy. Reference for every knob:
[`docs/Advanced_settings/WEB_PROXY.en.md`](../../docs/Advanced_settings/WEB_PROXY.en.md).

## 0. What you need

- a dedicated hostname (`proxy.example.com`) with an `A` record pointing at the
  server, resolving from outside your network;
- public inbound TCP 80 and 443 (80 is only for ACME and the HTTPS redirect);
- a static site that genuinely belongs to you, or an HTTP application on a
  loopback port;
- the telemt secrets you already hand to users — no new secret is needed.

Publish an internationalised hostname in its ACE (`xn--…`) form: clients derive
the bridge capability from the A-label, and a hand-typed Unicode host can map
differently across client platforms and then derive a different capability.

## 1. Install the binary

```bash
install -m 0755 telemt /usr/bin/telemt
install -d -o telemt -g telemt -m 0700 /var/lib/telemt
```

The shipped unit (`contrib/systemd/telemt.service`) already runs as the
`telemt` user with `/etc/telemt/telemt.toml`. No unit change is required for
the WEB transport: both relay listeners are loopback and need no capability.

## 2. Provide the public site

The repository deliberately ships no site. If many operators deployed the same
starter page, its body and assets would become an easy active-probing
signature. Use your own.

```bash
install -d -o telemt -g telemt /var/lib/telemt/site
# copy your generated site there; index.html is required, 404.html recommended
```

The site is read into memory once at start-up — restart telemt after changing
files. For a database-backed site, a CMS, SSE, or WebSockets, run the
application on a loopback port and use `public_upstream` instead of
`public_dir`.

## 3. Enable the transport

Append to `/etc/telemt/telemt.toml`:

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

Every user in `[access.users]` can now use the WEB transport with the secret
they already have. Start with `https`; move to `https-lanes` or
`websocket-lanes` once the deployment is healthy and the front proxy serves
HTTP/2.

## 4. Front proxy

Copy one of the templates in this directory and substitute the hostname:

- [`Caddyfile.example`](Caddyfile.example) — automatic certificates, HTTP/2,
  WebSocket pass-through, no extra configuration;
- [`nginx.conf.example`](nginx.conf.example) — for a host that already runs
  nginx; certificates come from certbot or your own automation.

The front proxy must forward **every** path to telemt. There is no separately
hosted relay path, so an unauthenticated prober only ever sees your site.

Never enable request-header logging: the session bearer travels in a header on
the WebSocket upgrade.

## 5. Firewall

Allow 80/443 from anywhere and keep the relay private:

```bash
ufw allow 80/tcp
ufw allow 443/tcp
ufw deny 8080/tcp
ufw deny 8081/tcp
```

The provider's network firewall is the second required boundary — the relay
listeners must never be reachable from outside the host.

## 6. Verify

On the server:

```bash
systemctl restart telemt && systemctl --no-pager --full status telemt caddy
curl --fail http://127.0.0.1:8081/healthz
curl --fail http://127.0.0.1:8081/readyz
curl --silent http://127.0.0.1:8081/metrics | head
ss -lntp | grep -E '8080|8081|443'
```

From your own machine:

```bash
curl -sSI https://proxy.example.com/ | head -3          # your site, HTTP 200
curl -sS  https://proxy.example.com/nope -o /dev/null -w '%{http_code}\n'   # 404
```

A valid capability is the only thing that reveals the bridge:

```bash
# capability = base64url-no-pad(HMAC-SHA256(secret, "tdesktop-web-proxy-bridge-v1\n" + host))
python3 - <<'PY'
import base64, hmac, hashlib
host, secret_hex = "proxy.example.com", "000102030405060708090a0b0c0d0e0f"
mac = hmac.new(bytes.fromhex(secret_hex),
               ("tdesktop-web-proxy-bridge-v1\n" + host).encode(), hashlib.sha256)
print(f"https://{host}/?bridge=" + base64.urlsafe_b64encode(mac.digest()).decode().rstrip("="))
PY
```

Fetching that URL must return an HTML document with a
`Content-Security-Policy: … script-src 'nonce-…'` header. Any other query on
`/` must return your ordinary index.

Clients need nothing but the hostname and their existing secret — a WEB-capable
app derives the same capability locally and never exposes the secret to the
page.

## 7. Watch

```text
tproxy_sessions_live            # on 127.0.0.1:8081/metrics
tproxy_streams_live
tproxy_streams_rejected_total   # rising: a limit is too tight
tproxy_limit_hits_total
tproxy_backend_dial_failures_total   # only meaningful for loopback backends
```

The same series appear on telemt's own metrics endpoint with the
`telemt_web_` prefix.

## Rollback

Set `enabled = false` under `[web]` and restart. The transport is additive:
nothing else in telemt changes when it is off.
