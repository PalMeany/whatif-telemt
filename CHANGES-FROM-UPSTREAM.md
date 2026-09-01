# Changes from upstream Telemt

This is the description of changes required by §2 of the TELEMT PUBLIC LICENSE
3.3 (`LICENSE:80-81`). See [NOTICE.md](NOTICE.md) for the attribution and
modification statement, and [LICENSE](LICENSE) for the full terms.

**WhatIf Telemt is an unofficial, modified version of Telemt, not affiliated
with or endorsed by the Telemt project.**

## Fork point

This repository was forked from <https://github.com/telemt/telemt> at commit
`d851200`, which is **telemt 3.4.25**. Everything below is a change made in
this fork after that commit. For the exact delta as it stands now, run
`git diff --stat d851200..HEAD`, or see
<https://github.com/PalMeany/whatif-telemt/compare/d851200...main>.

## Upstream base

The fork tracks upstream releases by merging them. It currently carries
**telemt 3.5.5** (`ac71d92`), merged whole: the configuration `types/` and
`load/` split, the `proxy/handshake` decomposition, `proxy/authenticated`, the
`transport/socket` fragmented-send work, `pf` support in the SYN limiter, the
api `users/` and `config_store/` splits, include-graph-aware config revisions,
the tier-2 warm-writer fallback in middle-proxy selection, and the per-user
metric `_total` suffixes are all upstream's.

Upstream 3.5.x added a **WEB proxy of its own**, written separately against the
same published protocol. The two share that protocol and nothing else. Both are
present in this build: upstream's under `[web]`, bound as a
`[[server.listeners]]` entry with `transport = "web"`, and this fork's under
`[fork.web]` with its own listener. `fork.web_implementation` selects between
them, and either or both can run. Upstream's WEB module needs nothing from the
listener subsystem this fork replaced, so it is wired to this fork's accept
loop instead of to upstream's `ListenerManager`.

Upstream's hot listener-rebind subsystem
(`maestro/listeners/{plan,bind,control,accept}`, `resolve_reload_config`, and
`PATCH /v1/config` for `server.listeners`) is still not carried, because it is
built on the maestro this fork replaced. Upstream's activation-gated
`spawn_config_watcher` is also not taken: it is upstream's fix for the same
lost-write race this fork closes with a config snapshot hash threaded through
the reload command, and running both is untested.

None of the work below has been submitted to, reviewed by, or accepted by the
upstream maintainers.

## The WEB proxy transport and its carriers

The headline addition is a WEB proxy transport (`src/web/**`,
`src/config/web/**`, documented in
[docs/Advanced_settings/WEB_PROXY.en.md](docs/Advanced_settings/WEB_PROXY.en.md)).
It implements the client-independent WEB proxy protocol v1 wire contract of
`telegramdesktop/tproxy-server`, so a WEB-capable Telegram app keeps its normal
MTProxy framing and sends every proxy connection through one app-owned WebView
carrier that looks, to anyone who cannot authenticate, like an ordinary HTTPS
website. Four carrier modes exist — `https`, `https-lanes`, `websocket` and
`websocket-lanes` — of which `https` and `https-lanes` are driveable by a
released client today, while the two WebSocket carriers have not been verified
against a shipping client, so the relay now names the affected profiles at
start-up rather than letting a mode mismatch present as a healthy carrier that
passes no data. The significant divergence from the reference relay is that
streams are terminated **in-process**: instead of forwarding each demultiplexed
stream to a stock MTProxy over loopback TCP, the bytes go straight into
telemt's existing client pipeline, so fake-TLS handling, Middle-End routing,
per-user limits, quotas, IP tracking, statistics and masking all apply to WEB
clients unchanged, and two syscalls and two kernel copies per chunk disappear.
The loopback backend remains available for deployments that want a separate
MTProxy process. Alongside it, bridge capabilities are now derived for every
secret form a WEB client can actually present — the plain 32-hex secret and its
`dd` random-padding form — while `ee` fake-TLS secrets, which the carrier
cannot carry and which clients refuse outright, are rejected at configuration
load, the `Host` check normalises case and ports (so a CDN or front proxy no
longer produces a site-wide 404 with no explanation), and an `X-Forwarded-For`
list is resolved to its last entry instead of being rejected.

## Conformance and hardening

The bulk of the work is closing the gaps between "the relay answers" and "the
relay is indistinguishable from an ordinary static origin, and a client actually
gets through". Every refused request now leaves the same trace as an ordinary
miss: reserved and ordinary paths drain their bodies through one bounded path,
so the connection survives and the timing matches; the `Host` check runs after
the target is classified and the target is percent-decoded once, closing two
routes by which an unauthenticated client could reach the operator's own
application with a live session bearer; and the Go-flavoured `404 page not
found` body and 502 banner that identified the origin internet-wide are gone.
The pending-frame budget was reworked so the control class reserves against its
own subtotal — previously one legal batch of stream opens could exhaust the pool
and kill the session on its own refusal frame — and a refused stream now
receives a CLOSE for its own id while the session and its other streams keep
running, as the protocol requires. Liveness and lifecycle were tightened
throughout: every write and writer lock is bounded, WebSocket fragment
reassembly is RFC 6455-correct, close codes are truthful, carrier lanes are
created only after every check that can still refuse them and are unwound and
bounded, a stream parked on global backpressure retries on a release signal
rather than a 25 ms timer, rotated or revoked `[[web.profiles]]` secrets now
take effect on reload instead of relaying until restart, and a WEB start-up
failure is fatal instead of a single warning line that left `systemctl
is-active` green while every client got a 502. The `max_sessions_per_ip` and
`max_bootstraps_per_ip` ceilings were defaulted back off, because counting live
sessions punishes a client on a flapping network and treats a carrier-grade NAT
as one user. The bridge document is pinned by SHA-256 against the reference
carrier, with `contrib/web/check-bridge-parity.sh` for re-verification when the
reference is bumped, because a silent one-character drift in 18 KB of minified
JavaScript produces a client-visible failure with no server-side symptom.
Separately from the WEB work, the in-process runtime reload subsystem
introduced in 3.4.25 was hardened against a full review: the API reload path now
validates, `failure_policy=rollback` actually restores the previous file,
reloads observe the shutdown token under a deadline, retired generations no
longer leak Middle-End writer tasks or TLS cache reservations, and the
admission budget, buffer pool and DNS overrides are correctly process- or
generation-scoped. The WEB test suite grew from nothing to cover budget
conservation, lane ceilings, probing parity, WebSocket ping and fragmentation,
and real MTProto handshakes driven end to end over the shared carrier, the
lane carrier and a lane WebSocket.

## Deployment tooling

Deploying the WEB transport needs a TLS front proxy that owns port 443, which
the stock quick-start does not account for, so this fork adds a from-scratch
runbook ([contrib/web/DEPLOY.md](contrib/web/DEPLOY.md)) covering port layout,
DNS, building, service account and unit, base and `[fork.web]` configuration, front
proxy, firewall, start-up order, verification, observability, troubleshooting,
updates and rollback. It ships front-proxy templates for Caddy
(`contrib/web/Caddyfile.example`) and nginx (`contrib/web/nginx.conf.example`)
that overwrite rather than append `X-Forwarded-For`, set no read timeout shorter
than the long-poll period, serve a site-shaped error page so a stopped relay
does not answer with a default banner, and deliberately omit compression and
HSTS — HSTS writes a device-side record that this host was contacted, and
compression is a length side channel over the operator's own responses. A
build-from-source Dockerfile (`contrib/web/Dockerfile.source`) and an unattended
installer (`install-web.sh`) automate the runbook on a clean Debian or Ubuntu
host, with a hardened systemd unit. The installer refuses to guess at the things
that quietly break a deployment: it will not invent a public site, because a
starter page shared across operators is an active-probing signature; it rejects
a non-canonical hostname, because the hostname is hashed into every bridge
capability; it refuses to put the direct MTProto listener on 443; it writes the
bridge capability to a root-owned `0600` file rather than to stdout; and a
failed health probe is fatal.

## The `[fork]` configuration section

Every fork-only feature is configured under `[fork]` and nowhere else
(`src/config/fork/**`, documented in
[docs/Fork/FORK_CONFIG.en.md](docs/Fork/FORK_CONFIG.en.md)). A configuration
written for stock telemt therefore keeps its exact meaning here, and deleting
the section leaves a working proxy.

`[fork.runtime]` carries one switch per runtime deviation from telemt — sixteen
of them, all defaulting to on — and `[fork] enabled = false` turns the lot off
in one key, so a fork-only behaviour can be bisected against upstream's without
editing anything else. Three deviations deliberately have no switch, because
"off" for them would mean reinstating deleted code rather than taking a
different branch: the single process-wide Direct copy-buffer controller,
generation-scoped `dns_overrides`, and the reload ticket's `Drop` safety net.
Product identification has none either, because TELEMT PUBLIC LICENSE 3.3 §3
requires a modified build to identify itself as unofficial.

This fork's WEB transport moved from `[web]` to `[fork.web]` when upstream's
`[web]` came back. A legacy fork-schema `[web]` is detected on the raw document
and migrated with a deprecation warning; a `[web]` mixing keys from both schemas
is refused rather than guessed at.

## Operator features

Four additions that upstream does not have, all off by default. Three live
under `[fork]`; the panel is deliberately top-level, for the reason given in
its entry below:

- **A built-in Prometheus panel** (`src/fork/prometheus/**`): one self-contained
  HTML document served next to the exposition this process already renders. It
  has no external reference of any kind — the page scrapes `/metrics` from its
  own origin and draws it client-side — so it works on a host with no outbound
  network and adds no dependency to the binary. It reads `# TYPE` rather than
  guessing from the name, because several series here are gauges despite a
  `_total` suffix, and it drops negative counter deltas, because a reload mints
  a fresh `Stats` and a reset is not a rate. By default it shares the metrics
  listener and its whitelist; `[fork.prometheus] listen` gives it one of its own
  serving the panel and nothing else.
- **A Telegram admin bot** (`src/fork/telegram/**`): a process-scoped task that
  long-polls the Bot API and answers a fixed command set. It writes through the
  same control plane and the same mutation lock as the HTTP API, so the two
  cannot race on the configuration file. Bot API traffic is routed through the
  configured `[[upstreams]]`, which matters more here than anywhere else in the
  tree: a host that needs a SOCKS or Shadowsocks egress to reach Telegram would
  otherwise have the bot dial `api.telegram.org` directly and announce the
  proxy's real address. Read-only until `allow_mutations` is set, and an update
  from a chat outside the admin list is dropped without a reply.
- **Bulk API requests** (`src/api/bulk/**`): `POST /v1/bulk` applies many user
  operations under one config load, one fsynced write and one set of runtime
  side effects, where the single-operation routes cost all three per operation
  and each write invalidates the caller's revision for the next request.
  Batches are atomic by default and refusals carry the same stable codes the
  single-operation routes use.
- **A built-in web panel** (`src/panel/**`, `src/config/panel/**`,
  `panel-ui/**`): an operator interface compiled into the binary — an embedded
  single-page application, a JSON API of its own, and a signed node-to-node
  endpoint that lets one node drive a fleet of others. It is a *client* of the
  Control API: every number it renders and every change it makes is a call to
  `[server.api]` on the node in question, nothing is cached between requests,
  and no proxy or protocol behaviour changes when it is on. Roles, RFC 6238
  second factors, a length-prefixed hash-chained audit log, a custom-header plus
  double-submit CSRF gate and two-dimension login throttling are all part of the
  surface, and it adds no Rust dependency: PBKDF2, TOTP, PEM decoding and the
  HTTP client are assembled from crates the proxy already uses.

  Its configuration is `[panel]` rather than `[fork.panel]`, unlike everything
  else here. The `[fork]` kill switch exists so one key turns off every
  behaviour that makes this build differ from stock telemt; the panel changes no
  such behaviour, and having `[fork] enabled = false` silently take away an
  operator's administrative interface is a worse failure than the inconsistency.
  It sits next to `[server.api]`, which it drives and which is top-level for the
  same reason.

---

## Кратко по-русски

Этот репозиторий — форк <https://github.com/telemt/telemt> от коммита
`d851200` (telemt 3.4.25). **Это неофициальная, изменённая версия Telemt, не
аффилированная с проектом Telemt и не одобренная им.** Форк подтягивает
вышестоящие релизы слиянием и сейчас основан на telemt 3.5.5 (`ac71d92`).
Ветка 3.5.x содержит собственную, написанную отдельно реализацию WEB-прокси.
Здесь присутствуют обе: апстримная в `[web]`, форковая в `[fork.web]`, а
`fork.web_implementation` выбирает, какая из них работает.

Что изменено в этом форке:

- **Транспорт WEB-прокси** (`src/fork/web/**`, `src/config/fork/web/**`): реализация
  клиентонезависимого протокола WEB proxy v1 из
  `telegramdesktop/tproxy-server`. Клиент Telegram сохраняет обычный формат
  MTProxy, но проводит все соединения через один WebView-носитель, который для
  внешнего наблюдателя выглядит как обычный HTTPS-сайт. В отличие от эталонного
  релея, поток завершается **внутри процесса**, а не пересылается стороннему
  MTProxy по loopback TCP, поэтому fake-TLS, Middle-End, лимиты, квоты,
  статистика и маскировка работают для WEB-клиентов без изменений.
  Capability выводятся для обеих форм секрета, которые принимает WEB-клиент
  (обычная 32-hex и её `dd`-форма); секреты `ee` fake-TLS отвергаются.
- **Соответствие протоколу и устойчивость**: любой отказ теперь неотличим от
  обычного «404» — и по телу ответа, и по времени, и по состоянию соединения;
  устранены два пути, по которым неаутентифицированный клиент мог достучаться до
  приложения оператора; переработан учёт очереди кадров; отказ в потоке больше не
  рвёт всю сессию; ограничены все операции записи; секреты из
  `[[fork.web.profiles]]` применяются при перезагрузке конфигурации; ошибка
  запуска WEB-транспорта стала фатальной; документ моста закреплён по SHA-256
  против эталона. Отдельно усилена подсистема перезагрузки конфигурации из
  3.4.25.
- **Раздел `[fork]`**: все функции форка настраиваются только в нём, поэтому
  конфиг от оригинального telemt сохраняет прежний смысл, а
  `[fork] enabled = false` выключает всё одним ключом. Справочник —
  [docs/Fork/FORK_CONFIG.ru.md](docs/Fork/FORK_CONFIG.ru.md).
- **Встроенная панель Prometheus, Telegram-бот администратора и bulk-запросы к
  API** — все выключены по умолчанию и настраиваются в `[fork]`.
- **Встроенная веб-панель** (`[panel]`, выключена по умолчанию): интерфейс
  оператора, вкомпилированный в бинарник — роли, второй фактор, журнал аудита с
  цепочкой хэшей и подписанная федерация узлов, при которой один сервер
  управляет остальными. Панель — клиент Control API: она ничего не кэширует и
  не меняет поведение прокси, поэтому её раздел вынесен на верхний уровень
  рядом с `[server.api]`, а не под `[fork]`. Справочник —
  [docs/Advanced_settings/PANEL.en.md](docs/Advanced_settings/PANEL.en.md).
- **Инструменты развёртывания**: пошаговый рунбук
  ([contrib/web/DEPLOY.md](contrib/web/DEPLOY.md)), шаблоны фронт-прокси для
  Caddy и nginx, Dockerfile для сборки из исходников и неинтерактивный
  установщик `install-web.sh` для Debian/Ubuntu.
