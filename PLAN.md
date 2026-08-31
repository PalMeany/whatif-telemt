# Fork feature configuration and telemt 3.5.5 upgrade

Working plan. Checked items are landed and compile.

## Goal

1. A stock telemt 3.5.5 `config.toml` loads and runs unchanged (backward compatibility).
2. Everything this fork adds on top of telemt lives in one `[fork]` section and can be
   switched off individually, settings included.
3. Both WEB proxy implementations are available and selectable: telemt's own
   (`[web]` + `transport = "web"`) and this fork's (`[fork.web]`).
4. New fork features, all off by default: built-in Prometheus panel, Telegram admin
   bot, bulk API requests.
5. The telemt base moves from 3.5.2 to 3.5.5.

## Phase 1 — telemt 3.5.5 base, WEB-independent parts

- [x] `Cargo.toml`: version 3.5.2 -> 3.5.5, plus `tokio-tungstenite` and `futures-util`.
- [x] `src/metrics.rs`: per-user counters gain the `_total` suffix (upstream rename).
- [x] `src/transport/middle_proxy/send/selection.rs`: tier-2 warm-writer fallback.
- [x] `src/maestro/generation.rs`: `try_spawn_session` split, `client_runtime_deps`
      restored (upstream's WEB backend needs it).
- [x] `src/config/hot_reload/watcher.rs`: `hot_changed` comparison fix.
- [x] Upstream's activation-gated `spawn_config_watcher` deliberately **not** taken:
      it is upstream's fix for the race this fork closes with the config snapshot
      hash, and running both is untested.

## Phase 2 — `[fork]` section, fork WEB moves under it

- [x] `src/config/fork/`: `ForkConfig` and every fork feature switch.
- [x] `src/config/web/` -> `src/config/fork/web/`, `ProxyConfig.web` -> `ProxyConfig.fork.web`.
- [x] `src/web/` -> `src/fork/web/`, `crate::web` -> `crate::fork::web`.
- [x] Legacy `[web]` written against the fork schema is migrated to `[fork.web]` with a
      deprecation warning; a `[web]` mixing both schemas is refused.
- [x] Strict keys, hot reload, `ProxyConfig::validate`, config tests.
- [x] Sixteen `[fork.runtime]` switches wired to their call sites.

## Phase 3 — telemt's own WEB transport restored

- [x] Upstream `src/web/`, `src/config/types/web*`, `src/config/load/{validate_web*,runtime_web}`,
      `src/api/web_{runtime,status}*` brought in at 3.5.5.
- [x] `transport = "web"` accepted again and dispatched from this fork's accept loop.
- [x] `WebIngress` owns the trace store, lifecycle publication and session manager.
- [x] `fork.web_implementation` selects, and refuses a contradicting configuration.

## Phase 4 — new fork features

- [x] `[fork.prometheus]`: self-contained HTML panel over the metrics listener, or its own.
- [x] `[fork.telegram]`: admin bot over the Bot API, routed through `[[upstreams]]`.
- [x] `[fork.api]`: `POST /v1/bulk`, one config write per batch.

## Phase 5 — surface

- [x] `config.toml` sample.
- [x] `docs/Fork/FORK_CONFIG.{en,ru}.md`, `docs/Config_params/*`, `docs/Advanced_settings/WEB_PROXY.en.md`.
- [x] `CHANGES-FROM-UPSTREAM.md` (required by TELEMT PUBLIC LICENSE 3.3 §2), READMEs.
- [x] `install-web.sh` and `contrib/web/DEPLOY.md` retargeted at `[fork.web]`.
- [x] Tests for every new config path and feature switch.
- [x] `cargo fmt`, `cargo clippy`, `cargo nextest run`.

## Phase 6 — adversarial review of this branch

Sixteen findings, verified in code and fixed in `0095f53`. The ones worth
naming, because each was a real defect rather than a style point:

- [x] The WEB trace-policy watcher spun at full CPU from the first hot reload,
      because re-cloning the published `RuntimeWatchState` rewinds the config
      receiver it parks on.
- [x] The Prometheus panel read six metric families this binary never emits, and
      its dedicated listener 404'd the `/metrics` the page scrapes.
- [x] The bot replied into whatever chat a command arrived in, so an admin could
      post a rotated secret to a shared group; and its Bot API failures were
      charged to the upstreams client traffic uses.
- [x] `me_writer_teardown`, `process_buffer_pool` and `reload_validate_candidate`
      did not do what their own documentation said.
- [x] Bulk: atomic aborts labelled rolled-back operations `ok`, the timeout could
      cancel after the write, effects ran outside the lock, and three operations
      diverged from the single-operation routes they mirror.

## Phase 7 — closing the gaps the review left open

- [x] End-to-end test that telemt's restored WEB transport is reachable through
      this fork's accept loop: a real TCP request on a `transport = "web"`
      listener is answered by the configured decoy, an unknown `Host` is not,
      and the implementation selector keeps the transport down when told to.
- [x] The Telegram bot driven against a local Bot API server: what it puts on
      the wire, how it reads a refusal, and that an oversized reply is split.
- [x] The batch decision layer split out of `run_batch` and tested directly:
      atomic abort, rollback relabelling, non-atomic partial application, a
      spent budget stopping before the write.
- [x] The panel's parsing assumptions pinned against a real rendered exposition,
      and its metric set pinned against the exporter in both directions.
- [x] Audited the 3.5.2 -> 3.5.5 reconciliation by hand, since that review agent
      died on a session limit: `client_runtime_deps` is field-for-field
      upstream's, no file is registered by two modules, the API gates run before
      the new pre-match hooks, `web_runtime` honours `read_only`, a `None`
      snapshot hash means "do not suppress" rather than "suppress everything",
      and the hot-reload overlay matches upstream's plus the fork's section.
- [x] Documented the one bulk limitation left unfixed: a batch whose `access.*`
      tables are owned by different include files is refused wholesale, because
      one write cannot span two files.
- [x] Key-by-key `[fork]` reference in `CONFIG_PARAMS.{en,ru,de}.md`.

Not done, deliberately: no German narrative `FORK_CONFIG.de.md`. The fork's own
flagship document (`WEB_PROXY.en.md`) is English-only, so German operators get
the key-by-key table like every other key in the reference.

Not possible here: a Linux build. This toolchain has no `x86_64-unknown-linux-gnu`
std, so the netfilter SYN-limiter backends were never compiled. No line I changed
falls inside a `target_os = "linux"` region, which is checked mechanically rather
than by eye.

## Known failing tests, all pre-existing

- `proxy::direct_relay::security_tests::*` (4) and
  `proxy::direct_relay::subtle_adversarial_tests::*` (2) fail on the base commit too;
  they depend on `/tmp` symlink canonicalisation and file-lock behaviour that differ
  on macOS.
- `web::session::backend::tests::delayed_valid_handshake_reaches_the_authenticated_relay`
  fails on stock upstream 3.5.5 as well; it drives a real TCP accept under
  `start_paused`.
- `proxy::client::security_tests::idle_pooled_connection_closes_cleanly_in_client_handler_path`
  is flaky under parallel load and passes in isolation.
