# NOTICE

This file is the attribution and modification notice required by sections 1 and 2
of the TELEMT PUBLIC LICENSE 3.3 (`LICENSE:74-83`), which governs this
distribution.

## This is a modified, unofficial version

**WhatIf Telemt is an unofficial, modified version of Telemt. It is not
affiliated with, endorsed by, or supported by the Telemt project or its
maintainers.**

**WhatIf Telemt — это неофициальная, изменённая версия Telemt. Проект не
аффилирован с проектом Telemt и его сопровождающими, не одобрен и не
поддерживается ими.**

This software has been modified. A description of the changes is in
[CHANGES-FROM-UPSTREAM.md](CHANGES-FROM-UPSTREAM.md), as required by §2:

> If you modify the Software, you MUST clearly state that the Software has been
> modified and include a brief description of the changes made.
>
> Modified versions MUST NOT be presented as the original Telemt.
>
> — `LICENSE:80-83`

## Upstream project and fork point

| | |
|---|---|
| Original project | Telemt |
| Original repository | <https://github.com/telemt/telemt> |
| Fork point (commit) | `d851200` |
| Fork point (version) | telemt 3.4.25 |
| This repository | <https://github.com/PalMeany/whatif-telemt> |

Upstream Telemt has continued independently since that commit and is now on the
3.5.x line, which contains its own, separately written WEB proxy
implementation. The WEB proxy transport in this repository is not that
implementation and the two are not the same code. Nothing here should be read
as describing, representing, or speaking for upstream Telemt.

## Copyright and licence

The copyright notice carried by the licence accompanying this software, quoted
as it appears at `LICENSE:1-2`:

```
######## TELEMT LICENSE 3.3 #########
##### Copyright (c) 2026 Telemt #####
```

The full terms are in [LICENSE](LICENSE). The official translations referenced
by `LICENSE:22-24` are in [docs/LICENSE/](docs/LICENSE/). `LICENSE` and
everything under `docs/LICENSE/` are reproduced verbatim from upstream and are
not modified in this fork; §1 requires exactly that:

> Redistributions of the Software, in source or binary form, MUST RETAIN the
> above copyright notice, this license text, and any existing attribution
> notices.
>
> — `LICENSE:74-76`

Per `LICENSE:41-42` ("Recipients of the Software are granted rights only under
the License provided with the version of the Software they received"), the
`LICENSE` file shipped in this repository is what governs this distribution.
Note that the version table in [LICENSING.md](LICENSING.md) is upstream's and
covers versions 1.0 through 3.4.0 only; it does not list 3.4.25, the version
this fork branched from.

Use of the name "Telemt" in "WhatIf Telemt" is made under `LICENSE:94-96`,
which permits the name to describe a modified version "only if the modified
version is clearly identified as a modified or unofficial version". That
identification is the purpose of this file, and it is repeated wherever the
name appears. This licence grants no permission to use the Telemt logo or any
Telemt branding (`LICENSE:87-88`), and none is used here.

## Third-party material: telegramdesktop/tproxy-server

The WEB proxy transport in `src/web/**` implements the published WEB proxy
protocol v1 wire contract of
[`telegramdesktop/tproxy-server`](https://github.com/telegramdesktop/tproxy-server),
so that clients speaking that protocol work against this relay unchanged
(`docs/Advanced_settings/WEB_PROXY.en.md:8-12`). Beyond implementing the
protocol, the following material in this repository is derived from that
project's source, pinned at revision `2873a08`:

- **The bridge document is byte-identical.** The `DOCUMENT` constant at
  `src/web/bridge.rs:129-505` is byte-identical to the document in that
  project's `internal/bridge/page.go`, apart from one added 19-byte
  `<!--__PADDING__-->` node at `src/web/bridge.rs:502`. The embedded
  `<script>` span — 18,080 bytes, of which 18,044 bytes are JavaScript — matches
  exactly. Byte-identity is deliberate and enforced: `src/web/bridge.rs:126`
  pins the SHA-256 of the template, the test at `src/web/bridge.rs:512-533`
  fails the build on any drift, and `contrib/web/check-bridge-parity.sh`
  re-verifies it against a checkout of the reference.
- **Three string literals are copied verbatim**: the `Permissions-Policy` value
  at `src/web/bridge.rs:18` (408 bytes), the site
  `Content-Security-Policy` at `src/web/site.rs:17` (132 bytes, repeated in
  `contrib/web/Caddyfile.example:66` and `install-web.sh:433`), and the
  host-independent entries of the bridge CSP list at `src/web/bridge.rs:60-77`
  (270 bytes, same directives in the same order).
- **`contrib/web/Caddyfile.example` is derived from** that project's
  `deploy/Caddyfile`: 26 non-blank lines / 652 bytes survive verbatim, including
  the whole `handle_errors` block at `contrib/web/Caddyfile.example:64-72`. The
  comments were rewritten and several settings were deliberately reversed.
- **Four validation error strings** in `src/web/capability.rs` (lines 68, 86, 98,
  101) are copied from that project's `internal/config/config.go`, and three
  comments in `src/web/site.rs` (lines 5, 94, 160) are near-verbatim paraphrases
  of comments in its `internal/server/site.go`.
- **The relay is a close structural port**, not only an implementation of the
  wire contract: 52 function names recur, including choices no protocol
  document dictates. `src/config/web/mod.rs:3` records that the configuration
  "mirrors the tproxy-server reference configuration".
- **The `tproxy_*` metric prefix** rendered at `src/web/admin.rs:39` is
  deliberate parity with the reference's metric names, so reference dashboards
  keep working; the same values are exposed under `telemt_web_*` at
  `src/web/metrics.rs:145`.

Not derived from it: `src/web/frame.rs` is an independent Rust implementation of
the documented wire format with its own error type and its own test vectors
transcribed from the protocol specification, and `src/web/http/headers.rs`,
`contrib/web/nginx.conf.example`, `contrib/web/Dockerfile.source`,
`contrib/web/check-bridge-parity.sh` and `install-web.sh` are original.

**`telegramdesktop/tproxy-server` publishes no licence.** That repository
contains no `LICENSE`, `COPYING` or `NOTICE` file and no copyright notice in
any file. No permission of any kind has been granted in writing for the
material listed above, and this notice does not assert that any was. It is
recorded here as a statement of provenance so that anyone receiving this
software can see precisely what came from where and assess it for themselves.

## Other notes

- The translations table at `LICENSE:23` lists a German translation at
  `docs/LICENSE/TELEMT-LICENSE.de.md`. That file is not present in this
  distribution, and was not present at the fork point. `LICENSE` is reproduced
  unmodified, so the reference is left as upstream wrote it; `LICENSE:14-17`
  states that translations are informational and that the English version
  prevails.
- All Cargo dependencies of this software are MIT/Apache-2.0-class permissive
  licences. Nothing is vendored into this repository, and no dependency imposes
  an obligation on redistribution of this source.
- The additions made in this fork are offered on the same "AS IS" basis as the
  rest of the software, with no warranty of any kind, express or implied. See
  `LICENSE:157-164`.
