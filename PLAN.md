# Fork feature configuration and telemt 3.5.5 upgrade

Working plan. Checked items are landed in the working tree and compile.

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

- [ ] `Cargo.toml`: version 3.5.2 -> 3.5.5.
- [ ] `src/metrics.rs`: per-user counters gain the `_total` suffix (upstream rename).
- [ ] `src/transport/middle_proxy/send/selection.rs`: tier-2 warm-writer fallback.
- [ ] `src/maestro/generation.rs`: `try_spawn_session` split, `client_runtime_deps`
      restored (upstream's WEB backend needs it).
- [ ] `src/config/hot_reload/watcher.rs`: `hot_changed` comparison fix.

## Phase 2 — `[fork]` section, fork WEB moves under it

- [ ] `src/config/fork/`: `ForkConfig` and every fork feature switch.
- [ ] `src/config/web/` -> `src/config/fork/web/`, `ProxyConfig.web` -> `ProxyConfig.fork.web`.
- [ ] `src/web/` -> `src/fork/web/`, `crate::web` -> `crate::fork::web`.
- [ ] Legacy `[web]` written against the fork schema is detected and migrated to
      `[fork.web]` with a deprecation warning; a `[web]` mixing both schemas is refused.
- [ ] Strict keys, hot reload, `ProxyConfig::validate`, API config sections.

## Phase 3 — telemt's own WEB transport restored

- [ ] Upstream `src/web/`, `src/config/types/web*`, `src/config/load/{validate_web*,runtime_web}`,
      `src/api/web_{runtime,status}*` brought in at 3.5.5.
- [ ] `transport = "web"` accepted again and dispatched from the fork's accept loop.
- [ ] `WebTraceStore` / `WebRuntimeControl` owned by `run_telemt_core`, passed to `api::serve`.
- [ ] Both implementations refuse to bind the same address; `[fork] web_implementation`
      documents which one an operator asked for.

## Phase 4 — new fork features

- [ ] `[fork.prometheus]`: self-contained HTML panel over the existing metrics listener.
- [ ] `[fork.telegram]`: admin bot over the Telegram Bot API.
- [ ] `[fork.api]`: `POST /v1/bulk`, one config write and one reload per batch.

## Phase 5 — surface

- [ ] `config.toml` sample, `docs/Config_params/*`, `docs/Fork/*`.
- [ ] Tests for every new config path and feature switch.
- [ ] `cargo fmt`, `cargo clippy`, `cargo nextest run`.
