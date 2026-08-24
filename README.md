# WhatIf Telemt

**WhatIf Telemt is an unofficial, modified version of [Telemt](https://github.com/telemt/telemt) — not affiliated with, endorsed by, or supported by the Telemt project or its maintainers.**

An MTProto proxy in Rust and Tokio, forked from <https://github.com/telemt/telemt> at commit `d851200` (telemt 3.4.25) and developed independently since.

[README на русском](README.ru.md)

## What this is

Upstream Telemt implements the official Telegram MTProto proxy protocol with a
large set of production features on top of it. This fork inherits all of that
and exists to carry the rest: things upstream does not have, has not added yet,
or never will. Work lands here when it is worth running in production and there
is no reason to wait for it to land anywhere else.

Inherited at 3.4.25 and belonging to the Telemt project: the MTProto modes
(classic, `dd` secure, `ee` fake-TLS with SNI fronting), TLS-fronting, the
Middle-End pool, replay protection, masking, quotas and per-user limits, the
Control API and the metrics surface.

Added here, and described in full in
[CHANGES-FROM-UPSTREAM.md](CHANGES-FROM-UPSTREAM.md):

- the **WEB proxy transport** — MTProto carried inside an app-owned WebView over
  a carrier that looks like an ordinary HTTPS website, with streams terminated
  in-process rather than handed to a separate MTProxy over loopback TCP;
- **conformance and hardening work** on that transport: every refusal is
  indistinguishable from an ordinary 404 in body, headers, timing and connection
  state; the bridge document is pinned by SHA-256 against the reference carrier;
  frame budgets, carrier lanes, WebSocket liveness and profile reloads are
  covered by tests;
- **deployment tooling** for it: a from-scratch runbook, Caddy and nginx
  front-proxy templates, a build-from-source Dockerfile and an unattended
  installer;
- **hardening of the 3.4.25 runtime reload subsystem**, which this fork found to
  be incomplete under review.

Upstream has continued independently since the fork point and is now on the
3.5.x line. The two codebases have diverged, and a feature present in one is not
necessarily present in the other; upstream's 3.5.x contains its own, separately
written WEB proxy implementation, which is not the code in this repository.
Report problems with this fork here — not to the Telemt project.

The binary identifies itself as this fork:

```console
$ telemt --version
WhatIfTelemt 3.4.25.1w
```

## The WEB proxy transport

Full reference:
[docs/Advanced_settings/WEB_PROXY.en.md](docs/Advanced_settings/WEB_PROXY.en.md).

A WEB-capable Telegram app keeps its normal MTProxy framing and encryption, but
sends every proxy connection through one app-owned WebView instead of raw TCP.
The WebView runs a same-origin HTTPS or WebSocket carrier against a hostname
that, to anyone who cannot authenticate, is an ordinary HTTPS site: the operator's
own pages are served on every path, and only `GET /?bridge=<valid capability>`
returns anything else.

The transport implements the client-independent wire contract of
[`telegramdesktop/tproxy-server`](https://github.com/telegramdesktop/tproxy-server)
(WEB proxy protocol v1), so a client that speaks that protocol works against this
relay unchanged. The provenance of the material derived from that project is
recorded in [NOTICE.md](NOTICE.md).

```text
Internet :443 -> TLS front proxy (Caddy/nginx) -> 127.0.0.1:8080 WEB carrier
                                                     |
                                                     +-> in-process MTProto client pipeline
                                                     +-> operator site (memory or loopback app)
```

The reference relay forwards each demultiplexed stream to a stock MTProxy over a
loopback TCP connection. Here the stream is terminated **in-process**: the bytes
enter the same client pipeline a direct TCP client uses, so a WEB client gets
fake-TLS handling, Middle-End routing, per-user limits, quotas, IP tracking,
statistics and masking exactly like every other client, with two fewer syscalls
and two fewer kernel copies per chunk. The loopback backend remains available
(`backend = "127.0.0.1:2398"`) for deployments that want a separate MTProxy
process.

### Carrier modes

| Mode | Shape | Trade-off |
| --- | --- | --- |
| `https` | One serialized uplink, one long-poll downlink | Simplest; a busy direction is bounded by roughly `carrier_batch_bytes / RTT` |
| `https-lanes` | One independent request lane per logical stream | Removes carrier head-of-line blocking; needs HTTP/2 at the public origin |
| `websocket` | One multiplexed WebSocket | Removes the stop-and-wait ceiling; no per-stream isolation |
| `websocket-lanes` | One WebSocket per logical stream | Best isolation; more sockets and TLS setups per session |

The mode is fixed at session creation and baked into the bridge page, so a
client needs no new setting when an operator changes it.

**Use `https`.** All four modes are implemented and tested, but protocol v1 has
no carrier negotiation, and no released client has been observed driving a
WebSocket carrier (as of 2026-08). A mode the client cannot drive fails silently:
the session is created, counters rise, and the client sits on "connecting"
forever. The relay names the affected profiles at start-up rather than letting
that pass for a healthy carrier.

### What an operator needs to know before deploying

- **A TLS-terminating front proxy is required.** The bridge page must be fetched
  over real HTTPS with a publicly trusted certificate for the configured
  hostname. Only the front proxy is public; the carrier (`listen`) and admin
  (`admin_listen`) listeners stay on loopback, and the carrier is plaintext HTTP
  by design. The front proxy owns 443, so the direct MTProto listener needs
  another port or another address.
- **`web.hostname` is the name clients type into their app**, because the bridge
  capability is `HMAC(secret, "…\n" + hostname)` — relay and client must derive
  it over the same name.
- **Clients configure a hostname and an ordinary MTProxy secret, nothing else.**
  The capability is derived locally:
  `base64url-no-padding(HMAC-SHA256(secret, "tdesktop-web-proxy-bridge-v1\n" + hostname))`,
  giving `https://<hostname>/?bridge=<capability>`.
- **`ee` fake-TLS secrets cannot be used over this transport** — a WEB client
  refuses them — so enable `classic` or `secure` in `[general.modes]` for WEB
  users, and hand direct clients the `ee` link separately.
- **The operator supplies the public site.** Nothing here generates one: a
  starter page shared between operators is an active-probing signature.

## Build

Rust 2024 edition; a current stable toolchain. The release profile uses fat LTO,
so the build wants roughly 2 GB of RAM and several minutes.

```bash
git clone https://github.com/PalMeany/whatif-telemt.git
cd whatif-telemt
cargo build --release --locked
./target/release/telemt --version        # WhatIfTelemt 3.4.25.1w
./target/release/telemt config.toml      # run in the foreground
```

Tests: `cargo test` (CI runs `cargo fmt -- --check`, `cargo clippy` and
`cargo nextest run`).

Container image built from this working tree, for hosts where the toolchain is
not wanted:

```bash
docker build -f contrib/web/Dockerfile.source -t whatif-telemt:local .
```

### There are no release downloads yet

`install.sh`, the root `Dockerfile` and `docker-compose.yml` are upstream's
tooling repointed at this repository: they fetch a release archive or a
container image from `PalMeany/whatif-telemt`. This fork publishes neither, so
none of those paths can install it today, and the one-command installer in the
inherited [Quick Start Guide](docs/Quick_start/QUICK_START_GUIDE.en.md) fails the
same way. Build from source, or use the WEB installer below.

## Install with the WEB transport

[`install-web.sh`](install-web.sh) automates steps 2–10 of
[contrib/web/DEPLOY.md](contrib/web/DEPLOY.md) on a clean Debian or Ubuntu host
with systemd: it builds the binary from the checkout, creates the service
account and directories, writes the configuration, installs Caddy as the TLS
front proxy, opens the firewall, starts both services, verifies the relay, and
writes the credentials and bridge URL to a root-owned `0600` file.

```bash
git clone https://github.com/PalMeany/whatif-telemt.git /usr/local/src/telemt
cd /usr/local/src/telemt
./install-web.sh --hostname proxy.example.com --site /path/to/your/site
```

Run it as root. It refuses to guess at what quietly breaks a deployment: it will
not invent a public site, rejects a hostname that is not already canonical
(the hostname is hashed into every bridge capability), refuses to put the direct
MTProto listener on 443, and treats a failed health probe as fatal.
`./install-web.sh --help` lists the options; the runbook describes everything the
script writes; steps 0, 1, 5 and 11 onwards remain yours — the port layout,
DNS and the provider firewall, the public site itself, and handing out the
credentials.

Front-proxy templates, if you are configuring one by hand:
[Caddyfile.example](contrib/web/Caddyfile.example),
[nginx.conf.example](contrib/web/nginx.conf.example). Both overwrite rather than
append `X-Forwarded-For`, keep read timeouts above the long-poll period, answer
a failed backend with a short banner-free error body instead of the front
proxy's own branded page, and deliberately omit HSTS and compression.

## Configuration

[`config.toml`](config.toml) in the repository root is a working starting point:
listeners, modes, masking, users, and a commented `[web]` block. The binary takes
the path as its only positional argument.

```
telemt [run|start|stop|reload|status] [OPTIONS] [config.toml]
```

`run` is the default; `start`/`stop`/`status` manage a daemonized process,
`reload` sends SIGHUP for an in-process configuration reload. `--log-level`
(`debug|verbose|normal|silent`), `--silent`, `--log-file`, `--pid-file` and
`--run-as-user` are documented under `telemt --help`; `RUST_LOG` overrides the
configured log level. `telemt healthcheck config.toml --mode liveness` is the
probe the container images use.

Every key is documented in
[docs/Config_params/CONFIG_PARAMS.en.md](docs/Config_params/CONFIG_PARAMS.en.md),
including the `[web]` section added by this fork; the `[web.limits]`,
`[web.timeouts]` and `[[web.profiles]]` keys are documented in
[docs/Advanced_settings/WEB_PROXY.en.md](docs/Advanced_settings/WEB_PROXY.en.md).

## Documentation

Added by this fork:

- [WEB proxy transport](docs/Advanced_settings/WEB_PROXY.en.md) — configuration,
  carrier modes, limits, observability, operational notes, and the deliberate
  differences from the reference relay
- [Deployment runbook](contrib/web/DEPLOY.md) — a clean server to a working
  proxy, step by step
- [`contrib/web/check-bridge-parity.sh`](contrib/web/check-bridge-parity.sh) —
  re-verifies the bridge document against a checkout of the reference carrier
- [CHANGES-FROM-UPSTREAM.md](CHANGES-FROM-UPSTREAM.md) — everything changed since
  the fork point

Inherited from upstream Telemt (their installer instructions notwithstanding):

- Quick start: [English](docs/Quick_start/QUICK_START_GUIDE.en.md) ·
  [Russian](docs/Quick_start/QUICK_START_GUIDE.ru.md) ·
  [OpenBSD](docs/Quick_start/OPENBSD_QUICK_START_GUIDE.en.md)
- Configuration reference:
  [English](docs/Config_params/CONFIG_PARAMS.en.md) ·
  [Russian](docs/Config_params/CONFIG_PARAMS.ru.md) ·
  [German](docs/Config_params/CONFIG_PARAMS.de.md)
- Architecture: [model](docs/Architecture/Model/MODEL.en.md) ·
  [Control API](docs/Architecture/API/API.md) ·
  [TLS fronting fidelity](docs/Architecture/Fronting-splitting/TLS_FRONT_PROFILE_FIDELITY.en.md) ·
  [Middle-End KDF](docs/Architecture/Middle-end/KDF-internals/MIDDLE-END-KDF.en.md)
- Tuning: [high load](docs/Advanced_settings/HIGH_LOAD.en.md) ·
  [tuning](docs/Advanced_settings/TUNING.en.md)
- Setup examples: [double hop over a VPS](docs/Setup_examples/VPS_DOUBLE_HOP.en.md) ·
  [double hop over Xray](docs/Setup_examples/XRAY_DOUBLE_HOP.en.md)
- FAQ: [English](docs/FAQ.en.md) · [Russian](docs/FAQ.ru.md)

## Licence and attribution

This software is distributed under the **TELEMT PUBLIC LICENSE 3.3**. The full
terms are in [LICENSE](LICENSE) and the official translations are in
[docs/LICENSE/](docs/LICENSE/); both are reproduced verbatim from upstream and
are not modified here.

- [NOTICE.md](NOTICE.md) — the attribution and modification notice required by
  §1 and §2: the fork point, the statement that this software is modified, the
  basis on which the Telemt name is used for it, and the provenance of
  third-party material. Read it before deploying or redistributing this
  software.
- [CHANGES-FROM-UPSTREAM.md](CHANGES-FROM-UPSTREAM.md) — the description of
  changes required by §2.

The licence grants no permission to use the Telemt logo or any Telemt branding
(§3), and none is used here. If you run this as a public network service, §7 asks
you to attribute Telemt somewhere user-visible — and requires that the
attribution not imply endorsement.

## Contributing

Issues and pull requests for this fork belong in this repository. Send anything
concerning the original project to <https://github.com/telemt/telemt>.

Patches should build, pass `cargo test`, be formatted with `cargo fmt`, and stay
scoped to one change. Changes to the WEB transport must keep the bridge document
byte-identical to the reference carrier — the pinned SHA-256 in
`src/web/bridge.rs` fails `cargo test` otherwise.

Note what §6 of the licence does with a contribution: unless you state
otherwise, submitting it licenses it under the same terms, and grants the rights
described in the licence both to recipients of this software **and to the Telemt
maintainers**. Contribute only what you are willing to have used on those terms.

[CONTRIBUTING.md](CONTRIBUTING.md) and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
are adapted from upstream's and rewritten for this fork. Issues and pull
requests belong here; upstream's tracker and chat are not support channels for
WhatIf Telemt.
