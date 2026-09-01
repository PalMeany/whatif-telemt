# A fleet behind one panel

Three machines: one control node operators sign in to, and two edge nodes that
carry the traffic. When it is done, adding a user on any of the three is a form
on one page, and the control node's audit log records which node it went to.

Nothing here needs a database, a message broker, or a second binary. Each node
already runs a panel; the control node simply calls the others.

Reference for every key used below:
[Advanced_settings/PANEL.en.md](../Advanced_settings/PANEL.en.md).
Templates to copy: [`contrib/panel/`](../../contrib/panel/).

## What you need first

- Three hosts with telemt installed. `control.example.com`,
  `edge-fra-1.example.com`, `edge-sin-1.example.com` below.
- A DNS name per node that the control node can resolve. An address works too;
  the certificate an agent presents is pinned by fingerprint, not by name.
- Clocks within a minute of each other. Signed cluster requests carry a
  timestamp and an agent refuses one outside `panel.cluster.clock_skew_secs`.
  `timedatectl status` on each host; if `System clock synchronized: no`, fix
  that before going further — it will otherwise look like a wrong key.

## 1. The control node

Copy [`contrib/panel/master.toml`](../../contrib/panel/master.toml) to
`/etc/telemt/config.toml` and replace every `CHANGE ME`. The parts that matter:

```toml
[server.api]
enabled = true
listen = "127.0.0.1:9091"
auth_header = "Bearer <head -c 32 /dev/urandom | base64>"
runtime_edge_enabled = true

[panel]
enabled = true
listen = "127.0.0.1:8443"
data_dir = "/etc/telemt/panel"
require_totp = false          # turned on at step 6

[panel.cluster]
enabled = true
role = "master"
node_name = "control"
```

Publish it with a TLS front proxy —
[`contrib/panel/Caddyfile.example`](../../contrib/panel/Caddyfile.example) or
[`nginx.conf.example`](../../contrib/panel/nginx.conf.example) — under a
hostname of its own. Do not reuse the hostname the WEB proxy transport serves:
that origin is supposed to look like an ordinary website, and this one carries
HSTS and a login form.

Start telemt and read the generated password:

```
$ systemctl start telemt
$ journalctl -u telemt -n 5 | grep -i panel
Panel bootstrap account created; the generated password is in this file
  path=/etc/telemt/panel/panel-bootstrap.txt username=admin
Panel endpoint: http://127.0.0.1:8443/

$ sudo cat /etc/telemt/panel/panel-bootstrap.txt
```

Sign in at `https://control.example.com/` as `admin`. The panel demands a new
password before anything else is reachable. Then delete the bootstrap file.

## 2. An edge node

Copy [`contrib/panel/agent.toml`](../../contrib/panel/agent.toml) to
`/etc/telemt/config.toml` on `edge-fra-1`. This node terminates TLS itself, so
mint a certificate:

```
$ sudo /path/to/contrib/panel/make-panel-cert.sh edge-fra-1.example.com /etc/telemt/panel
Certificate: /etc/telemt/panel/fullchain.pem
Private key: /etc/telemt/panel/privkey.pem
SHA-256:     a77c9d69…c022
```

Self-signed is the right answer here. The control node pins that exact
certificate by SHA-256, so the pin is the identity and no certificate authority
is involved. Note the fingerprint; you will see it again in a moment.

The keys that make this node linkable:

```toml
[panel]
enabled = true
listen = "0.0.0.0:8443"

[panel.tls]
enabled = true
cert_path = "/etc/telemt/panel/fullchain.pem"
key_path = "/etc/telemt/panel/privkey.pem"

[panel.cluster]
enabled = true
role = "agent"
node_name = "edge-fra-1"
advertise_url = "https://edge-fra-1.example.com:8443"
allow_from = ["203.0.113.10/32"]     # the control node's address
```

`allow_from` is a second gate in front of a surface that is already signed.
Leave it empty only if the control node's address is not stable.

Open port 8443 to the control node and nothing else:

```sh
ufw allow from 203.0.113.10 to any port 8443 proto tcp
```

Start telemt. The log line confirms the same fingerprint the script printed:

```
Panel TLS certificate loaded fingerprint=a77c9d69…c022
Panel endpoint: https://0.0.0.0:8443/ cluster=true role="agent"
```

## 3. Read the link token

Sign in to the **edge node's own panel** at `https://edge-fra-1.example.com:8443/`
— it has its own bootstrap account, printed to its own log. Go to **Fleet → This
node's link token → Reveal token** and copy the value:

```
telemt-node:eyJ2IjoxLCJpZCI6Im5vZGUtQlJRdXBzU3FjYl…
```

One opaque string, carrying four things: the node's identity, the URL to reach
it at, the HMAC key that signs every request, and the certificate fingerprint to
pin. That is why it is hidden until you ask for it, and why the page also prints
the fingerprint separately — compare it with what `make-panel-cert.sh` printed
before pasting anything.

## 4. Link it

Back on the control node: **Fleet → Link node**, paste the token, give it a name
and a tag, confirm.

The link is proven before it is stored. The control node signs a `hello`, the
agent answers with its identity, and the two are compared — so a token pasted
into the wrong panel, a node behind a closed port, or a rotated key all fail
here rather than at first use. The dialog reports the version the node answered
with and the clock difference it measured.

Repeat steps 2–4 for `edge-sin-1`.

## 5. Use it

The node switcher at the top of the sidebar now lists three nodes, and every
page follows it. Some things worth doing once:

- **Overview** shows a fleet table: uptime, connections, rejects and user count
  per node, refreshed every 30 seconds. A node that stops answering says so
  there before anyone clicks into it.
- **Users** on an edge node creates the user on *that* node's disk. There is no
  fleet-wide user list — each node owns its own `[access.users]`, and the panel
  is explicit about which one you are editing.
- **Audit** on the control node records every mutation with the node it went to,
  so `control.post /v1/users node=node-BRQ…` is the whole story of who added a
  user where.
- **Settings → Default node** picks what the interface opens on.

## 6. Close it down

Now that an administrator exists and is signed in:

1. **Account → Second factor → Enrol authenticator** on the control node. Save
   the ten recovery codes; they are shown once and only their hashes are kept.
2. Set `require_totp = true` in `[panel]` on the control node and restart. Doing
   it in this order avoids a first sign-in that demands enrolment and a password
   change on the same screen.
3. Create per-person accounts under **Operators** rather than sharing `admin`.
   `viewer` reads everything, `operator` also manages proxy users and quotas,
   `admin` also reaches configuration and node links.
4. Delete `panel-bootstrap.txt` on every node.
5. Confirm the audit chain verifies: **Audit → Verify chain**.

## When something is wrong

`node_unreachable` on the Fleet page — probe it with **⋯ → Probe now**; the
error shown is the transport's own. In order of likelihood: `advertise_url`
names an address the control node cannot route to, the firewall rule is missing,
the agent's certificate was replaced and no longer matches the pin, or
`allow_from` does not include the control node.

`stale_timestamp` — the clocks drifted apart by more than `clock_skew_secs`.
Fix the clocks rather than widening the window; the window is how long a
captured request stays replayable.

`unknown_node` from a node that is definitely linked — its link key was rotated,
or its `panel.json` was recreated and it has a new identity. Re-link with a fresh
token.

The rest is in
[Advanced_settings/PANEL.en.md § Troubleshooting](../Advanced_settings/PANEL.en.md#troubleshooting).

## What this does not do

- **No fleet-wide user provisioning.** Adding a user to five nodes is five
  actions, or five calls to the Control API. Each node owns its own
  configuration file and the panel does not pretend otherwise.
- **No configuration templating.** The Config page edits one node's file.
- **No agent-initiated connections.** The control node dials the agents, so an
  agent behind NAT with no reachable address cannot be linked.
- **No secret escrow.** Rotating a user's secret shows it once, on the node that
  minted it.
