# Built-in Web Panel

The panel is an operator interface compiled into the `telemt` binary. It serves a
single-page application, a JSON API of its own, and — when federation is enabled —
a signed node-to-node endpoint that lets one node manage a fleet of others.

The panel is a **client of the Control API**. Every number it renders and every
change it makes is a call to `[server.api]` on the node in question, so nothing
about the proxy's behaviour changes when the panel is on.

```
browser ──TLS──► panel (:8443) ──loopback──► Control API (:9091) ──► telemt runtime
                    │
                    └──signed HTTPS──► agent panel ──loopback──► its own Control API
```

## Quick start

```toml
[server.api]
enabled = true
listen = "127.0.0.1:9091"
auth_header = "Bearer <a long random string>"

[panel]
enabled = true
listen = "127.0.0.1:8443"
```

Start telemt and read the log:

```
Panel bootstrap account created; the generated password is in this file
  path=/etc/telemt/panel/panel-bootstrap.txt username=admin
Panel endpoint: http://127.0.0.1:8443/
```

Sign in as `admin` with that password. The panel forces a password change before
anything else becomes reachable, and the bootstrap file can then be deleted.

Put the listener behind a TLS front proxy, or terminate TLS in-process:

```toml
[panel]
enabled = true
listen = "0.0.0.0:8443"

[panel.tls]
enabled = true
cert_path = "/etc/telemt/panel/fullchain.pem"
key_path  = "/etc/telemt/panel/privkey.pem"
```

A non-loopback `panel.listen` without `panel.tls.enabled` is refused at start-up
unless `panel.trusted_proxies` names an off-host front proxy. The session cookie
is `HttpOnly; Secure; SameSite=Strict`, and a `Secure` cookie over plaintext on a
routable address is not a configuration the panel will accept.

## Configuration

### `[panel]`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | `bool` | `false` | Enables the panel listener. |
| `listen` | `string` (`IP:PORT`) | `127.0.0.1:8443` | Panel bind address. |
| `data_dir` | `string` | `<config dir>/panel` | Holds the store, audit log, and bootstrap credential. Falls back to `<general.data_path>/panel` when that is set. |
| `whitelist` | `CIDR[]` | `[]` | Source allowlist for the whole listener. Empty means any source. |
| `trusted_proxies` | `CIDR[]` | loopback | Front proxies allowed to assert a client address via `X-Forwarded-For`. |
| `control_api_url` | `string` | derived | Base URL of this node's Control API. Derived from `server.api.listen`, mapping an unspecified bind address onto loopback. Only `https`, or `http` to a loopback host, is accepted. |
| `control_api_token` | `string` | `server.api.auth_header` | `Authorization` value used against the Control API. |
| `session_ttl_secs` | `u64` | `43200` | Absolute session lifetime. Range `[60, 2592000]`. |
| `session_idle_timeout_secs` | `u64` | `1800` | Idle timeout. Range `[60, session_ttl_secs]`. |
| `max_sessions_per_operator` | `usize` | `8` | Concurrent sessions one operator may hold. Range `[1, 128]`. |
| `max_sessions_total` | `usize` | `512` | Concurrent sessions across all operators. |
| `login_max_attempts` | `u32` | `5` | Failures tolerated before a lockout. Range `[1, 100]`. |
| `login_lockout_secs` | `u64` | `900` | Lockout duration. Range `[1, 86400]`. |
| `password_min_length` | `usize` | `12` | Shortest accepted password. Range `[8, 256]`. |
| `password_hash_iterations` | `u32` | `600000` | PBKDF2-HMAC-SHA256 work factor. Range `[100000, 5000000]`. |
| `require_totp` | `bool` | `false` | Every operator must enrol a second factor before anything else is reachable. |
| `request_body_limit_bytes` | `usize` | `262144` | Maximum panel request body. Range `[1024, 8388608]`. |
| `max_connections` | `usize` | `256` | Concurrent panel connections. Range `[1, 65536]`. |
| `header_read_timeout_ms` | `u64` | `10000` | Deadline for one request head, and for the TLS handshake. |
| `request_timeout_ms` | `u64` | `30000` | Deadline for serving one request. |
| `audit_enabled` | `bool` | `true` | Records every mutating action into the hash-chained audit log. |
| `audit_retention_days` | `u64` | `90` | Days of rotated audit history retained. |
| `audit_max_bytes` | `u64` | `67108864` | Audit log size that triggers a rotation. |

### `[panel.tls]`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | `bool` | `false` | Terminates TLS inside the process. |
| `cert_path` | `string` | `""` | PEM certificate chain, leaf first. |
| `key_path` | `string` | `""` | PEM private key: PKCS#8, PKCS#1, or SEC1. |

Both files are checked for readability before the privilege drop, so a wrong path
fails at start-up rather than at the first request.

### `[panel.cluster]`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | `bool` | `false` | Enables federation. |
| `role` | `standalone` \| `master` \| `agent` \| `master-agent` | `standalone` | What this node does in a fleet. `enabled = true` requires a role other than `standalone`. |
| `node_name` | `string` | hostname | Display name in a master's node list. |
| `advertise_url` | `string` | `""` | Base URL a master uses to reach this node. `https`, or `http` to a loopback host. Required to produce a link token. |
| `allow_from` | `CIDR[]` | `[]` | Source networks allowed to reach `/cluster/v1`. Empty means any source; every request still has to carry a valid signature. |
| `request_timeout_ms` | `u64` | `10000` | Deadline for one outbound request to a linked node. Range `[1000, 120000]`. |
| `clock_skew_secs` | `u64` | `60` | Accepted clock difference between master and agent. Range `[5, 600]`. |
| `nonce_capacity` | `usize` | `8192` | Replay-window nonces retained. Range `[256, 1048576]`. |
| `poll_interval_secs` | `u64` | `30` | Interval between background health polls of linked nodes. Range `[5, 3600]`. |

`[panel]` is process-owned: the listener binds, the certificate loads, and the
store opens once at start-up. A configuration reload reports `panel` under
`deferred_process_fields`; applying a change needs a restart.

## Federation

A fleet has one **master** and any number of **agents**. There is no separate
agent binary and no persistent channel: every linked node already runs a panel,
and that panel's `/cluster/v1` endpoint is the whole remote surface.

### On the agent

```toml
[panel]
enabled = true
listen = "0.0.0.0:8443"

[panel.tls]
enabled = true
cert_path = "/etc/telemt/panel/fullchain.pem"
key_path  = "/etc/telemt/panel/privkey.pem"

[panel.cluster]
enabled = true
role = "agent"
node_name = "edge-fra-1"
advertise_url = "https://edge-fra-1.example.com:8443"
allow_from = ["203.0.113.10/32"]   # the master's address
```

Open **Fleet → This node's link token → Reveal token** and copy the value. The
token is one opaque string carrying the node identity, the reachable URL, the
HMAC link key, and the certificate fingerprint to pin.

### On the master

```toml
[panel.cluster]
enabled = true
role = "master"
node_name = "control"
```

Open **Fleet → Link node**, paste the token, and confirm. The master proves the
link before storing it: an unreachable node or a wrong key fails at paste time.

Every page then has a node selector, and every Control API call the panel makes
is routed to whatever it names.

### How a cluster request is authenticated

Each request carries four headers and an HMAC-SHA256 signature over a canonical
description of it:

```
TELEMT-CLUSTER-V1\n<method>\n<path+query>\n<target node id>\n<unix ms>\n<nonce>\n<sha256(body) hex>
```

| Header | Meaning |
| --- | --- |
| `X-Telemt-Node` | Identifier of the node the request is addressed to. |
| `X-Telemt-Timestamp` | Unix milliseconds; must be within `clock_skew_secs`. |
| `X-Telemt-Nonce` | Canonical unpadded base64url, spent once inside the replay window. |
| `X-Telemt-Signature` | The signature, unpadded base64url. |

The signature binds the method, the target, the node identity, the time, the
nonce, and the exact body bytes, so a captured request cannot be replayed against
a different route, a different node, or after its window closes. The signature is
checked **before** the nonce is spent, so an unauthenticated caller cannot burn
nonces a legitimate master will use.

Transport security is independent of that: the master pins the agent's leaf
certificate by SHA-256 when the link token carries one, which is what makes a
self-signed certificate on the agent a defensible configuration.

`POST /panel/api/nodes/link-token/rotate` mints a fresh link key on an agent.
Every existing link to it stops working and has to be re-established.

## Roles

| Role | Reads | Proxy users | Config, reloads | Node links, panel accounts |
| --- | --- | --- | --- | --- |
| `viewer` | yes | no | no | no |
| `operator` | yes | yes | no | no |
| `admin` | yes | yes | yes | yes |

The gate is applied over the Control API's own method-and-path surface, and a
route without an explicit rule is administrator-only. Changing an account's role,
password, or disabled flag ends that account's sessions immediately.

## Security model

**Authentication.** Passwords are PBKDF2-HMAC-SHA256 with a per-credential salt at
600 000 iterations by default, run on a blocking thread so a login never stalls a
runtime worker. A credential stored under an older work factor is rehashed on the
next successful sign-in. An unknown account still pays for one derivation, so
login latency is not an account-existence oracle.

**Second factor.** RFC 6238 TOTP, HMAC-SHA1, 30-second step, six digits, one step
of drift tolerated. Ten single-use recovery codes are minted on enrolment and only
their SHA-256 hashes are stored.

**Sessions.** In memory only — a restart signs everyone out. The registry is keyed
by the SHA-256 of the cookie, so it never holds a replayable bearer. Both an
absolute lifetime and an idle timeout apply, and both a per-operator and a global
ceiling evict the oldest session.

**CSRF.** Every panel API request must carry `X-Telemt-Panel: 1`, which a
cross-origin caller cannot set without a preflight the panel never answers. Every
state-changing request must additionally carry the session's `X-Telemt-Csrf`
token, compared in constant time. An `Origin` header, when present, must match
`Host`.

**Throttling.** Failed logins are counted per account name and per source address.
The account bucket stops a password being guessed from rotating addresses; the
address bucket stops one address sweeping a list of account names.

**Response hardening.** Every answer carries `Content-Security-Policy`
(`default-src 'none'`, no inline script, no external origin), `X-Frame-Options:
DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`Cross-Origin-Opener-Policy`, `Cross-Origin-Resource-Policy`, a
`Permissions-Policy` denying every powerful feature, and `Cache-Control:
no-store`. Over TLS it also carries `Strict-Transport-Security`.

**Audit.** Every mutating action appends one JSON line whose SHA-256 covers both
the record and its predecessor's hash. Deleting or editing a line breaks
verification from that point on. **Audit → Verify chain** recomputes it across
every retained segment. Reads are deliberately not recorded.

**Files.** The store and the audit log are written through a same-directory
temporary file and a rename, with `0600` permissions in a `0700` directory. The
panel warns at start-up if the store is readable beyond its owner.

**Relay scope.** Only the documented `/v1` Control API surface is relayed, and a
path containing `.` or `..` segments is refused. The browser cannot be turned into
a generic request forwarder aimed at whatever else answers on the Control API's
address.

## Panel API

Everything under `/panel/api` requires a session cookie and the client header;
mutations additionally require the CSRF token.

| Method | Path | Role | Purpose |
| --- | --- | --- | --- |
| `POST` | `/panel/api/session` | — | Sign in. Body: `username`, `password`, optional `totp` or `recovery_code`. |
| `GET` | `/panel/api/session` | any | Current session. |
| `DELETE` | `/panel/api/session` | any | Sign out. |
| `GET` | `/panel/api/bootstrap` | any | Version, node identity, federation role, operator. |
| `POST` | `/panel/api/account/password` | any | Change own password; ends every other session. |
| `GET`/`POST`/`PUT`/`DELETE` | `/panel/api/account/totp` | any | Second-factor state, enrolment, confirmation, removal. |
| `GET`/`DELETE` | `/panel/api/account/sessions` | any | List own sessions, end all but the current one. |
| `GET`/`POST` | `/panel/api/operators` | admin | List and create panel accounts. |
| `PATCH`/`DELETE` | `/panel/api/operators/{id}` | admin | Role, disabled flag, password, second-factor reset, deletion. |
| `GET` | `/panel/api/nodes` | any | Local node plus every linked node, with health. |
| `POST` | `/panel/api/nodes` | admin | Link a node from a token. |
| `PATCH`/`DELETE` | `/panel/api/nodes/{id}` | admin | Rename, retag, repoint, unlink. |
| `POST` | `/panel/api/nodes/{id}/probe` | any | Probe one linked node now. |
| `GET` | `/panel/api/nodes/link-token` | admin | This node's link token (agent roles only). |
| `POST` | `/panel/api/nodes/link-token/rotate` | admin | Mint a new link key. |
| `GET` | `/panel/api/overview` | any | Cross-node summary. |
| `GET` | `/panel/api/audit` | admin | Recent audit records. |
| `GET` | `/panel/api/audit/verify` | admin | Recompute the hash chain. |
| `GET`/`PATCH` | `/panel/api/settings` | any / admin | Preferences; the fleet default node needs admin. |
| any | `/panel/api/control/v1/...` | by route | Relay to the selected node's Control API. |

The relay selects a node with `?node=<id>` or `X-Telemt-Panel-Node`. `local` and
this node's own identifier both mean the node the panel runs on. The Control API's
own envelope, including `revision`, is returned untouched.

`GET /healthz` needs no session and answers `ok` for a liveness probe.

## Building the interface

The bundle in `panel-ui/dist` is committed, so `cargo build` produces a binary
with the interface already embedded. To change it:

```bash
npm --prefix panel-ui ci
npm --prefix panel-ui run build
cargo build --release
```

`build.rs` walks `panel-ui/dist` and generates the asset table; a build without
that directory still produces a working panel API and serves a page saying the
bundle is missing.

For UI development against a running node:

```bash
npm --prefix panel-ui run dev   # proxies /panel/api to 127.0.0.1:8443
```

## Operational notes

- The panel needs `server.api.enabled = true`; the combination is refused at
  start-up with an explicit message.
- `data_dir` must be writable by the service account, and is created on first
  start **after** the privilege drop. The stock systemd unit already lists
  `/etc/telemt` under `ReadWritePaths`, which covers the default location; a
  hand-rolled deployment has to grant it explicitly, or the panel refuses to
  start with the path in the error.
- `server.api.read_only = true` renders every view and refuses every mutation. The
  configuration page says so rather than failing per action.
- Runtime-edge views — connection leaderboards, TLS fingerprints, recent events —
  need `server.api.runtime_edge_enabled = true` on the node in question.
- Deleting `panel.json` resets the panel: a new node identity, a new link key, and
  a fresh bootstrap account. Every existing link has to be re-established.
- Losing every administrator password means deleting `panel.json` and re-linking;
  there is no recovery path that does not go through the filesystem.
