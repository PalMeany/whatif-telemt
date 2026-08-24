# Issues
### Warnung

This is **WhatIf Telemt**, an unofficial, modified fork of [Telemt](https://github.com/telemt/telemt).
Issues and Pull Requests for this fork go to [this repository](https://github.com/PalMeany/whatif-telemt/issues).
The upstream Telemt issue tracker and chat are NOT support channels for this fork - do not report problems with this fork there.

***Each of your Issues triggers attempts to reproduce problems and analyze them, which are done manually by people***

Issues is **NOT** about:
- Question and Answer
- Helpdesk
- Configuration or Intergraion Support

---

# Pull Requests

### General
- ONLY signed and verified commits
- ONLY from your name
- DO NOT commit with `codex`, `claude`, or other AI tools as author/committer
- PREFER `flow` branch for development, not `main`

---

### Definition of Ready (MANDATORY)

A Pull Request WILL be ignored or closed if:

- it does NOT build
- it does NOT pass tests
- it does NOT follow formatting rules
- it contains unrelated or excessive changes
- the author cannot clearly explain the change

---

### Blessed Principles
- PR must build
- PR must pass tests
- PR must be understood by author

---

### AI Usage Policy

AI tools (Claude, ChatGPT, Codex, DeepSeek, etc.) are allowed as **assistants**, NOT as decision-makers.

By submitting a PR, you confirm that:

- you fully understand the code you submit
- you verified correctness manually
- you reviewed architecture and dependencies
- you take full responsibility for the change

AI-generated code is treated as **draft** and must be validated like any other external contribution.

The problem isn’t AI as a tool, but the dilution of responsibility. If the commit history says "Claude/GPT authored this", then who is accountable for the bug? Claude? GPT? Anthropic? OpenAI? Samuel Altman?

The user who didn’t read the diff? No one? But, in a sensitive system, *"no one"* is an unacceptable maintainer model.

PRs that look like unverified AI dumps WILL be closed

---

### Maintainer Policy

Maintainers reserve the right to:

- close PRs that do not meet basic quality requirements
- request explanations before review
- ignore low-effort contributions

Respect the reviewers time

---

### Enforcement

Pull Requests that violate project standards may be closed without review.

This includes (but is not limited to):

- non-building code
- failing tests
- unverified or low-effort changes
- inability to explain the change

These actions follow the Code of Conduct and are intended to preserve signal, quality, and this project's integrity

---

### Licensing of Contributions

This repository is governed by [TELEMT PUBLIC LICENSE 3.3](LICENSE), inherited from the
upstream fork point. See [LICENSING.md](LICENSING.md) and [NOTICE.md](NOTICE.md).

Making LICENSE §6 explicit, because it applies here exactly as it does upstream: unless you
state otherwise, any contribution you intentionally submit for inclusion is licensed under
TELEMT PUBLIC LICENSE 3.3. By submitting it you grant the rights described in that License,
with respect to your contribution, both to **all recipients of the Software** and to **the
Telemt maintainers** - not only to this fork. Contributions cannot be re-licensed here, and
this fork claims no rights over them beyond the ones §6 already grants.

If you are not willing to grant those rights, do not submit the contribution.
