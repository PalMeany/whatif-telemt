#!/bin/sh
# Unattended installer for telemt with the WEB proxy transport.
#
# Automates contrib/web/DEPLOY.md steps 2-9 on a clean Debian/Ubuntu host with
# systemd: builds the binary from this checkout, creates the service account,
# writes the configuration, installs Caddy as the TLS front proxy, and starts
# both services. The runbook remains the reference for anything this script
# refuses to guess at.
#
#   ./install-web.sh --hostname proxy.example.com --site ./mysite
#
# The stock install.sh cannot be used for this fork: it downloads a published
# release artifact, and this fork publishes none.

set -eu

HOSTNAME_ARG=""
SITE_DIR=""
USER_NAME="${USER_NAME:-alice}"
USER_SECRET=""
DIRECT_PORT="${DIRECT_PORT:-8443}"
CARRIER_PORT="${CARRIER_PORT:-8080}"
ADMIN_PORT="${ADMIN_PORT:-8081}"
CARRIER_MODE="${CARRIER_MODE:-https}"
SRC_DIR="${SRC_DIR:-}"
CONFIG_FILE="${CONFIG_FILE:-/etc/telemt/config.toml}"
STATE_DIR="${STATE_DIR:-/var/lib/telemt}"
BIN_PATH="${BIN_PATH:-/usr/bin/telemt}"
UNIT_FILE="${UNIT_FILE:-/etc/systemd/system/telemt.service}"
CADDYFILE="${CADDYFILE:-/etc/caddy/Caddyfile}"
SKIP_BUILD=0
SKIP_CADDY=0
SKIP_FIREWALL=0
FORCE=0

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'USAGE'
Usage: install-web.sh --hostname HOST --site DIR [options]

Required:
  --hostname HOST      Public hostname clients configure. Must already resolve
                       to this host: Caddy obtains a certificate for it.
  --site DIR           Operator-owned static site to serve. Needs index.html;
                       404.html is strongly recommended. This script generates
                       no site: a page shared by many operators is an
                       active-probing signature. Without --site everything is
                       installed and configured but nothing is started.

Options:
  --user NAME          Name of the generated proxy user       (default: alice)
  --secret HEX32       Use this secret instead of generating one
  --direct-port PORT   Direct MTProto listener, public        (default: 8443)
  --carrier-port PORT  WEB carrier listener, loopback         (default: 8080)
  --admin-port PORT    Relay health and metrics, loopback     (default: 8081)
  --carrier-mode MODE  https | https-lanes | websocket | websocket-lanes
                       Keep the default. The current client implements only the
                       HTTPS long-poll carrier and does not negotiate, so any
                       other mode hangs it on "connecting" with no error.
                                                              (default: https)
  --src DIR            Repository checkout to build from (default: this one)
  --config PATH        Configuration file  (default: /etc/telemt/config.toml)
                       STATE_DIR, BIN_PATH, UNIT_FILE, CADDYFILE and CRED_FILE
                       may be overridden the same way, through the environment.
  --skip-build         Use the already installed /usr/bin/telemt
  --skip-caddy         Do not install or configure the front proxy
  --skip-firewall      Do not touch ufw
  --force              Overwrite an existing configuration file
  -h, --help           Show this help

Everything this script writes is listed in contrib/web/DEPLOY.md.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --hostname) [ $# -ge 2 ] || die "--hostname needs a value"; HOSTNAME_ARG="$2"; shift 2 ;;
        --site) [ $# -ge 2 ] || die "--site needs a value"; SITE_DIR="$2"; shift 2 ;;
        --user) [ $# -ge 2 ] || die "--user needs a value"; USER_NAME="$2"; shift 2 ;;
        --secret) [ $# -ge 2 ] || die "--secret needs a value"; USER_SECRET="$2"; shift 2 ;;
        --direct-port) [ $# -ge 2 ] || die "--direct-port needs a value"; DIRECT_PORT="$2"; shift 2 ;;
        --carrier-port) [ $# -ge 2 ] || die "--carrier-port needs a value"; CARRIER_PORT="$2"; shift 2 ;;
        --admin-port) [ $# -ge 2 ] || die "--admin-port needs a value"; ADMIN_PORT="$2"; shift 2 ;;
        --carrier-mode) [ $# -ge 2 ] || die "--carrier-mode needs a value"; CARRIER_MODE="$2"; shift 2 ;;
        --src) [ $# -ge 2 ] || die "--src needs a value"; SRC_DIR="$2"; shift 2 ;;
        --config) [ $# -ge 2 ] || die "--config needs a value"; CONFIG_FILE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --skip-caddy) SKIP_CADDY=1; shift ;;
        --skip-firewall) SKIP_FIREWALL=1; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

# ---------------------------------------------------------------- validation

[ "$(id -u)" -eq 0 ] || die "run as root"
[ -n "$HOSTNAME_ARG" ] || { usage >&2; die "--hostname is required"; }
command -v systemctl >/dev/null 2>&1 || die "this script targets systemd hosts"
command -v openssl >/dev/null 2>&1 || die "openssl is required"

# The hostname is hashed into every bridge capability, so a client that types a
# different case or an added dot derives a different one and silently never
# reaches the bridge. Reject anything that is not already canonical.
case "$HOSTNAME_ARG" in
    *[!a-z0-9.-]*) die "hostname must be lowercase ASCII; publish an IDN in its xn-- form" ;;
    .*|*.|*..*) die "hostname '$HOSTNAME_ARG' is not a canonical name" ;;
    *.*) : ;;
    *) die "hostname '$HOSTNAME_ARG' must be fully qualified" ;;
esac

case "$CARRIER_MODE" in
    https) : ;;
    https-lanes|websocket|websocket-lanes)
        warn "carrier mode '$CARRIER_MODE' is not implemented by the current client: it will"
        warn "render the bridge, create a session, and then hang on \"connecting\" forever."
        warn "Use https unless you are testing a client that implements this mode."
        ;;
    *) die "carrier mode '$CARRIER_MODE' is not one of https, https-lanes, websocket, websocket-lanes" ;;
esac

for port in "$DIRECT_PORT" "$CARRIER_PORT" "$ADMIN_PORT"; do
    case "$port" in
        ''|*[!0-9]*) die "port '$port' is not a number" ;;
    esac
    [ "$port" -ge 1 ] && [ "$port" -le 65535 ] || die "port '$port' is out of range"
done
[ "$CARRIER_PORT" != "$ADMIN_PORT" ] || die "carrier and admin ports must differ"
[ "$DIRECT_PORT" != 443 ] || die "the front proxy owns 443; see DEPLOY.md step 0"

if [ -n "$USER_SECRET" ]; then
    case "$USER_SECRET" in
        *[!0-9a-fA-F]*) die "--secret must be hexadecimal" ;;
    esac
    [ "${#USER_SECRET}" -eq 32 ] || die "--secret must be exactly 32 hex characters"
    USER_SECRET=$(printf '%s' "$USER_SECRET" | tr 'A-F' 'a-f')
fi

if [ -n "$SITE_DIR" ]; then
    [ -d "$SITE_DIR" ] || die "site directory '$SITE_DIR' does not exist"
    [ -f "$SITE_DIR/index.html" ] || die "site directory '$SITE_DIR' has no index.html"
    [ -f "$SITE_DIR/404.html" ] || warn "site has no 404.html; the index will answer unknown paths"
fi

if [ -z "$SRC_DIR" ]; then
    SRC_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
fi
if [ "$SKIP_BUILD" -eq 0 ]; then
    [ -f "$SRC_DIR/Cargo.toml" ] || die "'$SRC_DIR' is not a telemt checkout; pass --src or --skip-build"
fi

if [ -e "$CONFIG_FILE" ] && [ "$FORCE" -eq 0 ]; then
    die "$CONFIG_FILE already exists; move it aside or pass --force"
fi

# Beside the configuration, because it holds the same secret in another form.
CRED_FILE="${CRED_FILE:-$(dirname -- "$CONFIG_FILE")/web-credentials.txt}"

if command -v getent >/dev/null 2>&1 && ! getent hosts "$HOSTNAME_ARG" >/dev/null 2>&1; then
    warn "$HOSTNAME_ARG does not resolve yet: Caddy cannot obtain a certificate until it does"
fi

say "Installing telemt + WEB transport for $HOSTNAME_ARG"

# --------------------------------------------------------------------- build

if [ "$SKIP_BUILD" -eq 1 ]; then
    [ -x "$BIN_PATH" ] || die "--skip-build was given but $BIN_PATH is not executable"
    say "Using the installed binary at $BIN_PATH"
else
    say "Installing build dependencies"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq build-essential pkg-config git curl ca-certificates >/dev/null

    if ! command -v cargo >/dev/null 2>&1; then
        say "Installing the Rust toolchain"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    fi
    # rustup installs under the invoking user's home; pick it up either way.
    if [ -f "${CARGO_HOME:-$HOME/.cargo}/env" ]; then
        # shellcheck disable=SC1091
        . "${CARGO_HOME:-$HOME/.cargo}/env"
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is still not on PATH after installing rustup"

    say "Building telemt (release profile uses fat LTO; this takes a few minutes)"
    ( cd "$SRC_DIR" && cargo build --release --locked )
    [ -f "$SRC_DIR/target/release/telemt" ] || die "the build produced no binary"
    install -m 0755 "$SRC_DIR/target/release/telemt" "$BIN_PATH"
fi
"$BIN_PATH" --version

# ------------------------------------------------------- account and layout

say "Creating the service account and directories"
groupadd --system telemt 2>/dev/null || true
useradd --system --gid telemt --home-dir "$STATE_DIR" \
        --shell /usr/sbin/nologin --no-create-home telemt 2>/dev/null || true

install -d -o telemt -g telemt -m 0700 "$STATE_DIR"
install -d -o root -g telemt -m 0750 "$(dirname -- "$CONFIG_FILE")"

# ---------------------------------------------------------------- the site

SITE_TARGET="$STATE_DIR/site"
install -d -o telemt -g telemt -m 0755 "$SITE_TARGET"
if [ -n "$SITE_DIR" ]; then
    say "Installing the public site into $SITE_TARGET"
    cp -R -- "$SITE_DIR"/. "$SITE_TARGET"/
    chown -R telemt:telemt "$SITE_TARGET"
else
    # Nothing is generated here and nothing is started later: see the refusal
    # before "Starting services". The directory is created either way so the
    # operator only has to copy a site into it.
    say "No public site supplied; preparing $SITE_TARGET for one"
fi

# --------------------------------------------------------------- the config

[ -n "$USER_SECRET" ] || USER_SECRET=$(openssl rand -hex 16)

say "Writing $CONFIG_FILE"
umask 077
cat > "$CONFIG_FILE" <<EOF
# Generated by install-web.sh.
# Reference: docs/Advanced_settings/WEB_PROXY.en.md and docs/Config_params.

[general]
# Refuse unknown keys instead of ignoring them: a mistyped limit in a file
# nobody reads back is a setting that silently never applied. The global
# default is false only so that upgrades of older configs keep working.
config_strict = true
use_middle_proxy = false
log_level = "normal"

[general.modes]
# A WEB client accepts only a plain or dd secret, and dd keeps the padded
# transform, so secure is what carries the WEB transport.
classic = false
secure = true
# tls serves direct fake-TLS clients; WEB clients never use it.
tls = true

[general.links]
show = "*"
public_host = "$HOSTNAME_ARG"
public_port = $DIRECT_PORT

[server]
port = $DIRECT_PORT

[[server.listeners]]
ip = "0.0.0.0"
port = $DIRECT_PORT

[server.api]
enabled = true
listen = "127.0.0.1:9091"
whitelist = ["127.0.0.1/32", "::1/128"]

[censorship]
tls_domain = "www.google.com"
mask = true

[access.users]
$USER_NAME = "$USER_SECRET"

[web]
enabled = true
hostname = "$HOSTNAME_ARG"
# Both relay listeners are plaintext and must stay on loopback: the front proxy
# owns TLS, and anything that reaches these ports reads the bridge capability
# and the session bearer.
listen = "127.0.0.1:$CARRIER_PORT"
admin_listen = "127.0.0.1:$ADMIN_PORT"
public_dir = "$SITE_TARGET"
carrier_mode = "$CARRIER_MODE"
# Every [access.users] entry gets WEB access with the secret it already has.
derive_user_profiles = true
trusted_proxies = ["127.0.0.0/8", "::1/128"]
EOF
umask 022
chown root:telemt "$CONFIG_FILE"
chmod 0640 "$CONFIG_FILE"

# ----------------------------------------------------- the bridge capability

# The capability is keyed by the secret bytes the user pastes into the app --
# including the dd prefix, which is part of the key. Derive it from the exact
# form handed out below or the printed URL is one no client ever requests.
HANDOUT_SECRET="dd$USER_SECRET"
CAPABILITY=$(printf 'tdesktop-web-proxy-bridge-v1\n%s' "$HOSTNAME_ARG" \
    | openssl dgst -sha256 -mac HMAC -macopt hexkey:"$HANDOUT_SECRET" -binary \
    | openssl base64 -A | tr '+/' '-_' | tr -d '=')

# A file, not stdout: a bridge URL echoed to a terminal survives in scrollback,
# in tmux logs and in pasted bug reports, and it is the whole credential.
say "Writing $CRED_FILE"
umask 077
cat > "$CRED_FILE" <<EOF
# Generated by install-web.sh for $HOSTNAME_ARG.
# Whoever holds these values holds the relay. Hand the host and the secret to
# the user over a channel you trust; the client derives the bridge URL itself.

host    $HOSTNAME_ARG
secret  $HANDOUT_SECRET
bridge  https://$HOSTNAME_ARG/?bridge=$CAPABILITY
EOF
umask 022
chown root:root "$CRED_FILE"
chmod 0600 "$CRED_FILE"

# ------------------------------------------------------------- the unit file

say "Installing the systemd unit"
install -d -m 0755 "$(dirname -- "$UNIT_FILE")"
if [ -f "$SRC_DIR/contrib/systemd/telemt.service" ]; then
    # Redirect rather than `sed -i`: the in-place flag takes an argument on BSD
    # sed and none on GNU sed, and this has to run on whatever the host ships.
    sed -e "s#^ExecStart=.*#ExecStart=$BIN_PATH $CONFIG_FILE#" \
        -e "s#^WorkingDirectory=.*#WorkingDirectory=$STATE_DIR#" \
        -e "s#^ReadWritePaths=.*#ReadWritePaths=$STATE_DIR#" \
        "$SRC_DIR/contrib/systemd/telemt.service" > "$UNIT_FILE"
    chmod 0644 "$UNIT_FILE"
else
    cat > "$UNIT_FILE" <<EOF
[Unit]
Description=Telemt
Wants=network-online.target
After=multi-user.target network.target network-online.target

[Service]
Type=simple
User=telemt
Group=telemt
WorkingDirectory=$STATE_DIR
ExecStart=$BIN_PATH $CONFIG_FILE
Restart=on-failure
RestartSec=10
LimitNOFILE=65536
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
UMask=0077
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
# WorkingDirectory is the only path the process writes: the tls_front cache and
# the quota state file are relative to it by default. Keep the two in step.
ReadWritePaths=$STATE_DIR
# AF_UNIX is not optional: censorship.mask and the tls_front fetcher both accept
# a unix socket, and the resolver talks to nscd over one.
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
# Nothing in the binary generates code at runtime.
MemoryDenyWriteExecute=true

[Install]
WantedBy=multi-user.target
EOF
fi
systemctl daemon-reload

# ----------------------------------------------------------------- the proxy

if [ "$SKIP_CADDY" -eq 1 ]; then
    warn "Skipping the front proxy: nothing terminates TLS until you configure one"
else
    if ! command -v caddy >/dev/null 2>&1; then
        say "Installing Caddy"
        export DEBIAN_FRONTEND=noninteractive
        apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl gnupg >/dev/null
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
            | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
            > /etc/apt/sources.list.d/caddy-stable.list
        chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg
        chmod o+r /etc/apt/sources.list.d/caddy-stable.list
        apt-get update -qq
        apt-get install -y -qq caddy >/dev/null
    fi

    say "Writing $CADDYFILE"
    install -d -m 0755 "$(dirname -- "$CADDYFILE")"
    [ ! -f "$CADDYFILE" ] || cp "$CADDYFILE" "$CADDYFILE.bak"
    if [ -f "$SRC_DIR/contrib/web/Caddyfile.example" ]; then
        sed -e "s/proxy\.example\.com/$HOSTNAME_ARG/g" \
            -e "s#127\.0\.0\.1:8080#127.0.0.1:$CARRIER_PORT#g" \
            "$SRC_DIR/contrib/web/Caddyfile.example" > "$CADDYFILE"
    else
        cat > "$CADDYFILE" <<EOF
{
	# Keeps Caddy's admin API off 127.0.0.1:2019, and keeps Caddy on TCP: with
	# HTTP/3 enabled it binds UDP/443 -- a port this installer never opens --
	# and stamps \`Alt-Svc: h3=":443"\` on every response.
	admin off
	servers {
		protocols h1 h2
		timeouts {
			read_header 10s
			read_body 60s
		}
	}
}

$HOSTNAME_ARG {
	# No \`encode\` and no Strict-Transport-Security, both deliberate:
	# compression is a length side-channel over the operator's own responses,
	# and HSTS records this host permanently on the client device.

	# Every path goes to the relay: the bridge, the transport endpoints, and
	# the operator's own site are all served by telemt through one origin.
	reverse_proxy 127.0.0.1:$CARRIER_PORT {
		# Long polls park for web.timeouts.long_poll_ms (25s by default) and
		# WebSocket carriers stay open for the whole session, so no read
		# timeout may be shorter than that. telemt bounds this hop itself.
		transport http {
			read_timeout 0s
			write_timeout 0s
		}
	}

	# A stopped relay answers like an ordinary site whose backend is down,
	# not with Caddy's own 502 page.
	handle_errors {
		header {
			Cache-Control "no-store"
			Content-Security-Policy "default-src 'self'; style-src 'self'; img-src 'self'; worker-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
			Permissions-Policy "camera=(), microphone=(), geolocation=()"
			Referrer-Policy "strict-origin-when-cross-origin"
			X-Content-Type-Options "nosniff"
			X-Frame-Options "DENY"
		}
		respond "{http.error.status_code} {http.error.status_text}" {http.error.status_code}
	}
}
EOF
    fi
    caddy validate --config "$CADDYFILE" --adapter caddyfile >/dev/null \
        || die "the generated Caddyfile did not validate"
fi

# -------------------------------------------------------------- the firewall

if [ "$SKIP_FIREWALL" -eq 1 ]; then
    warn "Skipping the host firewall; $CARRIER_PORT and $ADMIN_PORT must never be reachable from outside"
elif command -v ufw >/dev/null 2>&1; then
    say "Configuring ufw"
    ufw allow 22/tcp >/dev/null
    ufw allow 80/tcp >/dev/null
    ufw allow 443/tcp >/dev/null
    ufw allow "$DIRECT_PORT"/tcp >/dev/null
    ufw deny "$CARRIER_PORT"/tcp >/dev/null
    ufw deny "$ADMIN_PORT"/tcp >/dev/null
    ufw --force enable >/dev/null
else
    warn "ufw is not installed; make sure $CARRIER_PORT and $ADMIN_PORT stay unreachable from outside"
fi

# ------------------------------------------------------------------- start

if [ -z "$SITE_DIR" ]; then
    UNITS="telemt"
    [ "$SKIP_CADDY" -eq 1 ] || UNITS="telemt caddy"
    cat >&2 <<EOF

$(warn "No public site was supplied, so nothing has been started.")
  A generated page is not a cover story: every install would share one body
  shape, and the shape is what an active prober matches. Only a site that
  genuinely belongs to you makes this origin uninteresting.

  Copy yours into $SITE_TARGET -- index.html is required, 404.html is strongly
  recommended, see contrib/web/DEPLOY.md step 5 -- and then:

    chown -R telemt:telemt $SITE_TARGET
    systemctl enable --now $UNITS

  Everything else is in place: $CONFIG_FILE, $UNIT_FILE and $CRED_FILE, which
  holds the user's host, secret and bridge URL. The site is read into memory
  once at start-up, so restart telemt after changing it.
EOF
    exit 1
fi

say "Starting services"
systemctl enable --now telemt >/dev/null 2>&1 || true
systemctl restart telemt
[ "$SKIP_CADDY" -eq 1 ] || { systemctl enable --now caddy >/dev/null 2>&1 || true; systemctl restart caddy; }

# Give the relay a moment to bind before probing it.
sleep 2
systemctl is-active --quiet telemt || {
    journalctl -u telemt -n 30 --no-pager || true
    die "telemt did not start; see the log above"
}

# ------------------------------------------------------------------ verify

say "Verifying the relay"
command -v curl >/dev/null 2>&1 || die "curl is not installed; the relay cannot be verified"

curl --fail --silent --max-time 5 "http://127.0.0.1:$ADMIN_PORT/healthz" >/dev/null \
    || die "healthz did not answer on 127.0.0.1:$ADMIN_PORT; check journalctl -u telemt"
printf '    healthz  ok\n'

# A relay that never reports ready serves the front proxy's error page to every
# client, which is not a state to leave an operator believing is a success.
ready=0
attempt=0
while [ "$attempt" -lt 20 ]; do
    if curl --fail --silent --max-time 5 "http://127.0.0.1:$ADMIN_PORT/readyz" >/dev/null; then
        ready=1
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
[ "$ready" -eq 1 ] || {
    journalctl -u telemt -n 30 --no-pager || true
    die "the relay did not become ready after 20 tries; see the log above"
}
printf '    readyz   ready\n'

cat <<EOF

$(say "Done")

  The host, the secret and the bridge URL are in $CRED_FILE, mode 0600. Hand
  the user the host and the secret; the client derives the bridge URL itself.
  A WEB client accepts only a plain or dd-prefixed secret and refuses ee
  fake-TLS secrets outright, so hand out the dd form the file records.

  Any query on / other than the bridge capability returns your ordinary index.

  Check it from outside the host:

    curl -sSI https://$HOSTNAME_ARG/ | head -3
    curl -sS  https://$HOSTNAME_ARG/nope -o /dev/null -w '%{http_code}\\n'

  The bridge must be fetched with a GET -- it is issued only for GET /, so
  curl -I returns the ordinary index and looks like a broken relay:

    curl -sS -D - -o /dev/null "\$(sed -n 's/^bridge  //p' $CRED_FILE)" \\
      | grep -i 'content-security-policy'
    # expect default-src 'none' ... script-src 'nonce-...'

  Logs and metrics:

    journalctl -u telemt -f
    curl -s http://127.0.0.1:$ADMIN_PORT/metrics
EOF
