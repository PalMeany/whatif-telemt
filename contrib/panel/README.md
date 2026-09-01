# Panel deployment templates

Ready-to-edit configurations and front-proxy templates for the built-in web
panel. Reference: [docs/Advanced_settings/PANEL.en.md](../../docs/Advanced_settings/PANEL.en.md).
Step-by-step fleet walkthrough:
[docs/Setup_examples/PANEL_FLEET.en.md](../../docs/Setup_examples/PANEL_FLEET.en.md).

| File | For |
| --- | --- |
| [`standalone.toml`](standalone.toml) | One node, one operator interface, no federation. |
| [`master.toml`](master.toml) | The control node operators sign in to. |
| [`agent.toml`](agent.toml) | An edge node driven by that control node. |
| [`Caddyfile.example`](Caddyfile.example) | TLS front proxy, certificate obtained automatically. |
| [`nginx.conf.example`](nginx.conf.example) | TLS front proxy for a host that already runs nginx. |
| [`make-panel-cert.sh`](make-panel-cert.sh) | Self-signed certificate for an agent that terminates TLS itself. |

Every value that must be replaced is marked `CHANGE ME`.

## Which shape do you want

**One node.** `standalone.toml` plus one of the front-proxy templates. The panel
listens on loopback; the proxy publishes it under a hostname of its own.

**Several nodes, one place to manage them.** `master.toml` on the control node,
`agent.toml` on each edge node. Operators only ever sign in to the control node.

There are two ways to give an agent TLS, and they differ in what the master
trusts:

- **The agent terminates TLS itself** (`agent.toml` as written). Run
  `make-panel-cert.sh` to mint a certificate; the master pins it by SHA-256 from
  the link token. No certificate authority is involved and the pin is the
  identity. Replacing the certificate invalidates the pin and every master has
  to be re-linked.
- **A front proxy terminates TLS** on the agent, using a publicly trusted
  certificate. Set `panel.listen` to loopback, `panel.tls.enabled = false`, and
  `panel.cluster.advertise_url` to the public `https://` URL. The link token then
  carries no fingerprint and the master validates through web PKI, which is the
  right choice when certificates rotate on their own.

## Two panels, one word

`[fork.prometheus]` is also called a panel in this repository. It serves one
read-only page of metrics on the metrics listener, by default at the path
`/panel`, and has nothing to do with these files. The two are independent and
can run on the same host.

## Before the first sign-in

1. Replace every `CHANGE ME`, in particular `server.api.auth_header`
   (`head -c 32 /dev/urandom | base64`) and `censorship.tls_domain`.
2. Make `panel.data_dir` writable by the service account. It is created after
   the privilege drop, so a root-owned `/etc/telemt` with no write bit for the
   account stops the panel from starting. The stock systemd unit already lists
   `/etc/telemt` under `ReadWritePaths`.
3. Start telemt and read the generated password out of
   `<data_dir>/panel-bootstrap.txt`. The panel forces a password change before
   anything else is reachable; delete the file afterwards.
4. Enrol a second factor, then set `panel.require_totp = true` and restart. Doing
   it in that order avoids a first sign-in that demands enrolment and a password
   change on the same screen.

## Checking the deployment

```sh
# Liveness, no session required.
curl -fsS https://panel.example.com/healthz

# The application shell.
curl -fsS -o /dev/null -w '%{http_code}\n' https://panel.example.com/

# Cross-origin refusal, which proves the Origin check survives the front proxy.
curl -fsS -o /dev/null -w '%{http_code}\n' \
  -H 'X-Telemt-Panel: 1' -H 'Origin: https://evil.example.com' \
  https://panel.example.com/panel/api/session   # expect 403
```

On an agent, from the master's host, with the values from the link token:

```sh
# Reachability only. An unsigned request is refused, which is the correct
# answer: it proves the endpoint is up and that it is not open.
curl -sS -o /dev/null -w '%{http_code}\n' \
  https://edge-fra-1.example.com:8443/cluster/v1/hello   # expect 401
```
