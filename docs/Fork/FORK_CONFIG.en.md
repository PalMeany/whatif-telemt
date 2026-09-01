# The `[fork]` configuration section

Everything this fork adds on top of telemt is configured under `[fork]` and
nowhere else. Two consequences follow, and they are the point of the section:

- **A configuration written for stock telemt keeps its exact meaning.** No key
  outside `[fork]` behaves differently because this is a fork, so a telemt
  3.5.5 `config.toml` can be dropped in unchanged.
- **Deleting `[fork]` leaves a working proxy.** Every fork feature is reachable
  only through this section, and `[fork] enabled = false` turns the lot off in
  one key.

The section is optional. Leaving it out keeps the behaviour this fork has
always had, because every switch defaults to on.

## Contents

- [Top level](#top-level)
- [`[fork.runtime]` — runtime behaviour switches](#forkruntime--runtime-behaviour-switches)
- [`[fork.web]` — this fork's WEB proxy transport](#forkweb--this-forks-web-proxy-transport)
- [`[fork.prometheus]` — the built-in metrics page](#forkprometheus--the-built-in-metrics-page)
- [`[fork.telegram]` — the admin bot](#forktelegram--the-admin-bot)
- [`[fork.api]` — bulk requests](#forkapi--bulk-requests)
- [Which WEB proxy runs](#which-web-proxy-runs)
- [Upgrading from a pre-`[fork]` configuration](#upgrading-from-a-pre-fork-configuration)
- [What lives outside `[fork]`](#what-lives-outside-fork)
- [What is not switchable](#what-is-not-switchable)

## Top level

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch. `false` makes the process behave like stock telemt: every runtime switch below reads as off, and the fork's WEB transport, panel, bot and bulk API stay down regardless of their own `enabled` keys. |
| `web_implementation` | string | `"auto"` | Which WEB proxy runs: `auto`, `telemt`, `fork`, `both`, `off`. See [below](#which-web-proxy-runs). |

`enabled = false` keeps the rest of the section on disk. The keys an operator
wrote are preserved and reported by `GET /v1/config`; they simply do not take
effect. That is what makes the switch usable for bisecting a fork-only change
against upstream behaviour without editing anything else.

## `[fork.runtime]` — runtime behaviour switches

One boolean per deviation from stock telemt, all defaulting to `true`. Setting
one to `false` restores what telemt does. Every one of them is read at start-up
or on a reload, so changing one requires a runtime reload rather than a hot
config apply.

| Key | Default | On | Off (telemt behaviour) |
| --- | --- | --- | --- |
| `process_admission_budget` | `true` | One process-wide connection semaphore, retuned in place on reload. | Each generation mints its own, so a drain reload admits up to twice `server.max_connections` while both are live. |
| `process_buffer_pool` | `true` | One relay buffer pool for the process. | Each generation allocates its own, doubling committed relay memory for the length of a drain. |
| `process_uptime_clock` | `true` | `telemt_uptime_seconds` measures the process. | A reload reseeds it and uptime restarts from zero. |
| `reload_cancel` | `true` | `DELETE /v1/system/reload/{id}` cancels an in-flight reload. | A long drain holds the single reload slot until it finishes; both submit paths answer 409 meanwhile. |
| `reload_deadlines` | `true` | Runtime preparation, quiesce and the shutdown command grace are bounded. | Those steps retry without a deadline, and a wedged preparation can turn SIGTERM into SIGKILL. |
| `reload_config_rollback` | `true` | `failure_policy = "rollback"` restores the previous config file, revision-gated. | Only the candidate runtime is discarded; the written config stays on disk and the watcher hot-applies it. |
| `reload_validate_candidate` | `true` | A reload candidate is run through `ProxyConfig::validate()`. | Only the loader's own checks run, so a config that is fatal at start-up can be installed by a reload. |
| `reload_error_kind` | `true` | Reload status carries a stable `error_kind` slug next to `error`. | Telemt's shape: a message only. |
| `reload_config_snapshot_hash` | `true` | The new generation's watcher is seeded from the snapshot the reload was built from. | The watcher re-reads the file when it starts, losing a write that landed during preparation. |
| `me_writer_teardown` | `true` | Retired middle-end writer tasks are cancelled and their sockets dropped. | Writers are only signalled, and their connections accumulate across reloads. |
| `tls_front_cache_budget_release` | `true` | A retired TLS-front cache returns its full-certificate reservations to the process gauge. | The budget ratchets across reloads until every IP is refused. |
| `synlimit_generation_reconciler` | `true` | SYN-limiter rules are re-reconciled on a cutover and on a hot reload. | Rules are reconciled once at start-up and never again. |
| `shutdown_unbind_listeners_first` | `true` | Listening sockets are unbound as the first shutdown action. | Listeners stop late, so the port keeps completing TCP handshakes and resetting them for the whole shutdown window. |
| `session_admission_closed_metric` | `true` | `telemt_session_admission_closed_total` is exported. | The series is absent. |
| `user_delete_forgets_quota` | `true` | A deleted user's process-scoped quota and stats are dropped. | A re-created username starts pre-charged. |
| `rust_log_survives_reload` | `true` | An operator's `RUST_LOG` filter survives a reload. | A reload re-derives the filter from `general.log_level` alone. |

## `[fork.web]` — this fork's WEB proxy transport

The full reference is
[docs/Advanced_settings/WEB_PROXY.en.md](../Advanced_settings/WEB_PROXY.en.md).
The schema is unchanged from when this transport owned `[web]`; only the
section name moved.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Runs the transport. |
| `listen` | string | `"127.0.0.1:8080"` | Carrier listener. Plaintext: keep it behind a TLS front proxy. |
| `admin_listen` | string | `"127.0.0.1:8081"` | `/healthz`, `/readyz`, `/metrics`. Empty disables it. Must be loopback. |
| `hostname` | string | `""` | Public hostname clients configure. Must match the front proxy certificate. |
| `public_dir` | string | unset | Operator-owned static site, loaded once at start-up. Needs `index.html`. |
| `public_upstream` | string | unset | Loopback HTTP application serving the public site instead. Exactly one of the two is required. |
| `carrier_mode` | string | `"https"` | `https`, `https-lanes`, `websocket`, `websocket-lanes`. |
| `derive_user_profiles` | bool | `false` | Gives every `[access.users]` entry WEB access with its existing secret. |
| `trusted_proxies` | list | `["127.0.0.0/8", "::1/128"]` | Front proxies allowed to assert a client address. |
| `limits`, `timeouts` | table | see reference | Process ceilings and carrier deadlines. |
| `profiles` | array | `[]` | Explicit capability profiles. |

## `[fork.prometheus]` — the built-in metrics page

> Historically called "the panel", and its default path is still `/panel`. It is
> **not** the operator interface: that is the top-level `[panel]` section,
> documented in
> [Advanced_settings/PANEL.en.md](../Advanced_settings/PANEL.en.md). This one is
> a read-only metrics page. The two are independent and can run together.

One self-contained HTML document served next to the exposition this process
already renders. It carries no external reference: the page scrapes `/metrics`
from its own origin and draws it client-side, so it works on a host with no
outbound network.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Serves the panel. |
| `path` | string | `"/panel"` | Path the document is served on. Must not shadow `/metrics` or `/beobachten`. |
| `listen` | string | `""` | Dedicated listener as `ip:port`. Empty shares the metrics listener. |
| `whitelist` | list | loopback | Networks allowed to reach a dedicated listener. Ignored while `listen` is empty. |
| `refresh_secs` | int | `5` | Seconds between browser-side scrapes. |
| `history_points` | int | `120` | Samples the browser keeps per series. Maximum 1440. |
| `title` | string | `""` | Heading. Empty uses the product name and version. |
| `show_users` | bool | `false` | Renders per-user rows. Off by default because those series carry usernames as labels. |

With `listen` empty the panel inherits `server.metrics_whitelist` and the
metrics listener's connection budget, which is the common case. Setting
`listen` gives the panel its own address serving the panel and nothing else,
for a deployment where the page should be reachable from a workstation but
`/metrics` and `/beobachten` should not.

## `[fork.telegram]` — the admin bot

A process-scoped task that long-polls the Bot API and answers a fixed command
set from the same control plane the HTTP API writes through.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Runs the bot. |
| `token` | string | `""` | Token from @BotFather. Treat the config file as secret material. |
| `admins` | list of int | `[]` | Telegram ids allowed to talk to it. Both the sender and the chat must appear here, so a private chat needs one user id and a group needs its (negative) chat id too. Required when enabled. |
| `allow_mutations` | bool | `false` | Permits the commands that change configuration. |
| `api_base` | string | `"https://api.telegram.org"` | Bot API origin, for a self-hosted Bot API server. |
| `poll_timeout_secs` | int | `25` | `getUpdates` long-poll timeout. 1..=50. |
| `request_timeout_secs` | int | `30` | Per-request timeout. Must exceed `poll_timeout_secs`. |
| `upstream_scope` | string | `""` | `[[upstreams]]` scope the bot's own traffic is routed through. Empty dials the Bot API directly. |

Commands:

| Command | Needs `allow_mutations` | Does |
| --- | --- | --- |
| `/status` | no | Product, uptime, connections, user count, which WEB transports are up. |
| `/users` | no | Configured users with live connections and quota use. |
| `/links <user>` | no | The `tg://` and `t.me` links for one user. |
| `/adduser <user>` | yes | Adds a user and returns their generated secret. |
| `/deluser <user>` | yes | Removes a user and everything keyed by their name. |
| `/enable <user>` | yes | Admits a user again. |
| `/disable <user>` | yes | Stops admitting a user and cancels their live sessions. |
| `/rotate <user>` | yes | Issues a fresh secret and cancels their live sessions. |

Two things worth knowing before enabling it:

- **Both the sender and the chat must be on the admin list**, and an update
  failing either check is dropped without a reply. Checking the sender alone
  would let an admin type `/rotate alice` in a shared group and have the bot
  post the new secret to everyone in it. Sharing the bot with a group is
  therefore deliberate: add that group's negative chat id to `admins`.
- **Bot API traffic is direct unless `upstream_scope` is set.** That isolation
  is deliberate: the unscoped upstreams are the ones client traffic uses, and a
  Bot API endpoint that is merely unreachable would otherwise mark them
  unhealthy after a handful of failed polls and degrade the proxy over a chat
  integration being down. On a host that needs an egress to reach Telegram at
  all, set `upstream_scope` and give the matching `[[upstreams]]` entry the same
  `scopes` value; only upstreams carrying that scope are ever selected, so their
  failures stay charged to them.

## `[fork.api]` — bulk requests

`POST /v1/bulk` applies many user operations under one config load, one write
and one set of runtime side effects. The single-operation routes cost all three
per operation, and each write invalidates the caller's revision for the next
request.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `bulk_enabled` | bool | `false` | Serves the route. |
| `bulk_max_operations` | int | `100` | Operations one batch may carry. Maximum 1000. |
| `bulk_timeout_secs` | int | `10` | Wall-clock budget for one batch. Must stay below the API listener's 15-second connection deadline, so 1..=14. |

Request:

```json
{
  "atomic": true,
  "operations": [
    { "id": "1", "action": "user.create", "body": { "username": "alice" } },
    { "id": "2", "action": "user.patch", "user": "bob", "body": { "data_quota_bytes": 1073741824 } },
    { "id": "3", "action": "user.disable", "user": "carol" },
    { "id": "4", "action": "user.rotate_secret", "user": "dave" },
    { "id": "5", "action": "user.delete", "user": "erin" }
  ]
}
```

`atomic` defaults to `true`: one refused operation aborts the batch, nothing is
written, and the response is `409` with a per-operation result list. With
`atomic = false` the valid operations are applied and the refused ones are
reported. `If-Match` is honoured once for the whole batch, which is strictly
better than N separate calls where each write invalidates the next caller's
revision.

Operation bodies are the same shapes the single-operation routes accept, so
`user.patch` keeps the tri-state semantics of `PATCH /v1/users/{user}`: an
absent field is unchanged, `null` removes it. `user.enable` removes the
`access.user_enabled` key rather than writing `true`, and `user.rotate_secret`
leaves live sessions alone, both matching the routes they mirror.

One limit is worth knowing before splitting a configuration across includes: a
batch writes every `access.*` table it touched in **one** file, so if the
tables it touched are owned by different includes it is refused with `409
config_patch_not_atomic` and nothing is written. The equivalent single-operation
calls would each succeed, because each one touches fewer tables. A batch
containing `user.delete` is unaffected — that operation already writes all eight
tables through either route — but a batch mixing, say, a create with a patch of
a quota table kept in a separate include is not. Keep the `access.*` tables in
one file, or split such a batch by file.

## Which WEB proxy runs

Two WEB proxy implementations exist in this binary. They speak the same
published protocol and share nothing else:

| | telemt's | this fork's |
| --- | --- | --- |
| Configured under | `[web]` | `[fork.web]` |
| Bound by | a `[[server.listeners]]` entry with `transport = "web"` | its own `listen` |
| Stream termination | in-process | in-process, or a loopback MTProxy |
| Public site | per-vhost decoy | `public_dir` or `public_upstream` |

`fork.web_implementation` decides which of them the process runs:

| Value | Effect |
| --- | --- |
| `auto` (default) | Runs whichever the rest of the configuration enables. Both, if both are configured. |
| `telemt` | Runs telemt's. Refuses `[fork.web] enabled = true`. |
| `fork` | Runs this fork's. Refuses a `transport = "web"` listener. |
| `both` | Runs both. |
| `off` | Runs neither, and refuses a configuration that asks for one. |

A contradiction is a load-time error rather than a silent override, because a
silent override is the failure an operator cannot see: the unit stays green
while the transport they configured never binds.

## Upgrading from a pre-`[fork]` configuration

Before telemt grew its own WEB transport, this fork owned `[web]` outright. An
existing configuration does not need editing: a `[web]` table written against
the fork's schema is detected on the raw document, moved to `[fork.web]`, and a
deprecation warning is logged. Renaming the section silences the warning.

The two schemas share only `enabled`, `limits` and `timeouts`; every other key
belongs to exactly one of them, so the decision is made on keys that cannot
appear in both. A `[web]` mixing keys from both schemas is refused with both
lists named, because guessing would bind the wrong transport.

## What lives outside `[fork]`

One thing: the built-in web panel, configured in the top-level `[panel]`
section. It is the single exception to the rule this document opens with, and it
is deliberate.

The `[fork]` kill switch exists so one key turns off every behaviour that makes
this build differ from stock telemt. The panel changes no such behaviour — it is
a *client* of the Control API, and every view it renders and every change it
makes is a call to `[server.api]`, which is itself top-level for the same
reason. Having `[fork] enabled = false` silently take away an operator's
administrative interface is a worse failure than the inconsistency, so `[panel]`
sits next to the API it drives rather than under the fork switch.

A configuration written for stock telemt still keeps its exact meaning: stock
telemt has no `[panel]` section, so nothing an operator already wrote changes
behaviour, and leaving the section out leaves the panel off.

Reference: [Advanced_settings/PANEL.en.md](../Advanced_settings/PANEL.en.md).
Templates: [`contrib/panel/`](../../contrib/panel/).

## What is not switchable

Three fork deviations have no switch, because "off" for them would mean
reinstating code this fork deleted rather than taking a different branch:

- the single process-wide Direct copy-buffer controller and its envelope;
- generation-scoped `dns_overrides` — the process-global override table is
  gone, and the snapshot is an explicit parameter on every path that resolves;
- the reload ticket's `Drop` safety net, which terminates a reload's status if
  an early return would otherwise wedge the API at 409 for the process lifetime.

Product identification is likewise not switchable. TELEMT PUBLIC LICENSE 3.3 §3
requires a modified build to identify itself as unofficial, so `--version`,
`telemt_build_info` and `GET /v1/system/info` always report this fork.
