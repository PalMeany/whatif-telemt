#!/bin/sh
# Mints a self-signed certificate for a telemt panel that terminates TLS itself.
#
# A self-signed certificate is a defensible choice for a cluster agent: the
# master pins it by SHA-256 from the link token, so the pin *is* the identity
# and no certificate authority is involved. It is not a good choice for a panel
# operators reach in a browser — use a publicly trusted certificate there, or a
# front proxy that obtains one.
#
#   ./make-panel-cert.sh edge-fra-1.example.com /etc/telemt/panel
#
# Prints the SHA-256 fingerprint the master pins, which is also what the link
# token carries, so the two can be compared by eye after linking.

set -eu

HOSTNAME_ARG="${1:-}"
OUT_DIR="${2:-/etc/telemt/panel}"
DAYS="${DAYS:-825}"

if [ -z "${HOSTNAME_ARG}" ]; then
    echo "usage: $0 <hostname-or-ip> [output-directory]" >&2
    exit 2
fi

command -v openssl >/dev/null 2>&1 || { echo "openssl is required" >&2; exit 1; }

CERT="${OUT_DIR}/fullchain.pem"
KEY="${OUT_DIR}/privkey.pem"

if [ -e "${CERT}" ] || [ -e "${KEY}" ]; then
    echo "refusing to overwrite existing ${CERT} or ${KEY}" >&2
    echo "move them aside first; replacing a certificate invalidates every pin" >&2
    exit 1
fi

mkdir -p "${OUT_DIR}"
chmod 700 "${OUT_DIR}"

# A bare IP has to go in the SAN as an IP entry, not a DNS one. The panel's own
# pinned verifier ignores names entirely, but a browser and any other client
# will not, and an operator will eventually point one at this host.
case "${HOSTNAME_ARG}" in
    *[!0-9.]*) SAN="DNS:${HOSTNAME_ARG}" ;;
    *)         SAN="IP:${HOSTNAME_ARG}" ;;
esac

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -keyout "${KEY}" -out "${CERT}" \
    -days "${DAYS}" -nodes \
    -subj "/CN=${HOSTNAME_ARG}" \
    -addext "subjectAltName=${SAN}" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=serverAuth" \
    >/dev/null 2>&1

chmod 600 "${KEY}" "${CERT}"

# The panel logs this same value at start-up, and the link token carries it.
FINGERPRINT="$(openssl x509 -in "${CERT}" -noout -fingerprint -sha256 \
    | cut -d= -f2 | tr -d ':' | tr 'A-F' 'a-f')"

cat <<TXT
Certificate: ${CERT}
Private key: ${KEY}
Valid for:   ${DAYS} days
SHA-256:     ${FINGERPRINT}

Point [panel.tls] at these files:

  [panel.tls]
  enabled = true
  cert_path = "${CERT}"
  key_path = "${KEY}"

Then restart telemt. The panel logs the same fingerprint, and the link token it
produces carries it, so a master that pastes that token pins this exact
certificate. Replacing the certificate later invalidates the pin and every
master has to be re-linked.
TXT
