#!/usr/bin/env bash
# Proves that our bridge document has not forked from the reference carrier.
#
# The document in src/web/bridge.rs is the client: roughly 18 KB of minified
# JavaScript implementing all four carriers inside the WebView. A silent fork
# breaks clients while the relay keeps answering 200/204, so the document is
# pinned by DOCUMENT_SHA256 and by this script. Run it by hand when bumping the
# reference; nothing in the build fetches anything over the network.
#
#   contrib/web/check-bridge-parity.sh /path/to/tproxy-server
#
# Exit status: 0 when the only difference is our deliberate padding node,
# 1 when the documents have actually diverged, 2 on a usage or setup error.

set -eu

PADDING_NODE='<!--__PADDING__-->'
PINNED_REVISION='2873a08'

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 2; }

usage() {
    cat <<'EOF'
usage: check-bridge-parity.sh <tproxy-server-checkout> [telemt-checkout]

Extracts the bridge document from the reference (internal/bridge/page.go) and
from ours (src/web/bridge.rs), diffs them, and reports whether the only
difference is the padding comment node this fork adds on purpose.
EOF
}

case "${1:-}" in
    -h | --help | '')
        usage
        exit 2
        ;;
esac

REFERENCE_DIR="$1"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OURS_DIR="${2:-$(cd "$SCRIPT_DIR/../.." && pwd)}"

REFERENCE_FILE="$REFERENCE_DIR/internal/bridge/page.go"
OURS_FILE="$OURS_DIR/src/web/bridge.rs"

[ -f "$REFERENCE_FILE" ] || die "not a tproxy-server checkout: $REFERENCE_FILE is missing"
[ -f "$OURS_FILE" ] || die "not a telemt checkout: $OURS_FILE is missing"

if command -v sha256sum >/dev/null 2>&1; then
    digest_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    digest_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "neither sha256sum nor shasum is available"
fi

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# Both documents live in a raw string literal, so the extraction is the same
# shape twice: print everything after the opening marker, stop at the closing
# delimiter on its own line.
extract_between() {
    awk -v marker="$2" -v terminator="$3" '
        !inside {
            start = index($0, marker)
            if (start > 0) {
                inside = 1
                print substr($0, start + length(marker))
            }
            next
        }
        $0 == terminator { exit }
        { print }
    ' "$1"
}

extract_between "$REFERENCE_FILE" 'const document = `' '`' >"$WORK_DIR/reference.html"
extract_between "$OURS_FILE" 'const DOCUMENT: &str = r####"' '"####;' >"$WORK_DIR/ours.html"

[ -s "$WORK_DIR/reference.html" ] || die "could not find 'const document' in $REFERENCE_FILE"
[ -s "$WORK_DIR/ours.html" ] || die "could not find 'const DOCUMENT' in $OURS_FILE"

script_span() {
    sed -n '/^<script nonce="__NONCE__">$/,/^<\/script>$/p' "$1"
}

script_span "$WORK_DIR/reference.html" >"$WORK_DIR/reference.js"
script_span "$WORK_DIR/ours.html" >"$WORK_DIR/ours.js"

[ -s "$WORK_DIR/reference.js" ] || die "no <script> span in the reference document"
[ -s "$WORK_DIR/ours.js" ] || die "no <script> span in our document"

if command -v git >/dev/null 2>&1 && [ -d "$REFERENCE_DIR/.git" ]; then
    REFERENCE_REVISION="$(git -C "$REFERENCE_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
else
    REFERENCE_REVISION="unknown"
fi

say "reference: $REFERENCE_FILE (revision $REFERENCE_REVISION, pinned $PINNED_REVISION)"
say "ours:      $OURS_FILE"
say "document:  $(wc -c <"$WORK_DIR/reference.html" | tr -d ' ') reference bytes, $(wc -c <"$WORK_DIR/ours.html" | tr -d ' ') ours"
say "script:    $(wc -c <"$WORK_DIR/reference.js" | tr -d ' ') reference bytes, $(wc -c <"$WORK_DIR/ours.js" | tr -d ' ') ours"

if [ "$REFERENCE_REVISION" != "unknown" ] && [ "$REFERENCE_REVISION" != "$PINNED_REVISION" ]; then
    warn "the checkout is at $REFERENCE_REVISION but src/web/bridge.rs pins $PINNED_REVISION;"
    warn "update PINNED_REVISION here and the doc comment on DOCUMENT_SHA256 together."
fi

STATUS=0

# The padding node is the one difference this fork ships knowingly: without it
# every deployment returns a globally constant Content-Length for GET /, which
# identifies a relay to a passive observer with nothing decrypted. Remove it
# before diffing so a real fork is not buried under a difference we chose.
PADDING_LINES="$(grep -c -x -F "$PADDING_NODE" "$WORK_DIR/ours.html" || true)"
grep -v -x -F "$PADDING_NODE" "$WORK_DIR/ours.html" >"$WORK_DIR/ours-normalized.html" || true

if [ "$PADDING_LINES" != "1" ]; then
    warn "expected exactly one $PADDING_NODE line in our document, found $PADDING_LINES"
    STATUS=1
fi

if diff -u "$WORK_DIR/reference.html" "$WORK_DIR/ours-normalized.html" >"$WORK_DIR/document.diff"; then
    say "document parity: identical to the reference apart from the padding node"
    say "  expected difference: one added line, $PADDING_NODE (deliberate, see"
    say "  the comment on padding() in src/web/bridge.rs) -- not a fork"
else
    printf '\033[1;31m[fork]\033[0m the bridge document has diverged from the reference:\n' >&2
    sed -e 's/^/    /' "$WORK_DIR/document.diff" >&2
    STATUS=1
fi

if diff -u "$WORK_DIR/reference.js" "$WORK_DIR/ours.js" >"$WORK_DIR/script.diff"; then
    say "script parity: the <script> span is byte-identical"
else
    printf '\033[1;31m[fork]\033[0m the <script> span has diverged from the reference:\n' >&2
    sed -e 's/^/    /' "$WORK_DIR/script.diff" >&2
    STATUS=1
fi

# Keep the in-tree pin honest: a document edit that forgot to update
# DOCUMENT_SHA256 fails `cargo test web`, and one that updated the constant
# without re-running this script is exactly what this script exists to catch.
OURS_DIGEST="$(digest_of "$WORK_DIR/ours.html")"
PINNED_DIGEST="$(sed -n 's/^const DOCUMENT_SHA256: &str = "\([0-9a-f]*\)";$/\1/p' "$OURS_FILE")"

if [ -z "$PINNED_DIGEST" ]; then
    warn "DOCUMENT_SHA256 not found in $OURS_FILE"
    STATUS=1
elif [ "$OURS_DIGEST" != "$PINNED_DIGEST" ]; then
    warn "DOCUMENT_SHA256 is stale: the document hashes to $OURS_DIGEST"
    warn "but src/web/bridge.rs pins $PINNED_DIGEST"
    STATUS=1
else
    say "DOCUMENT_SHA256 is current: $OURS_DIGEST"
fi

if [ "$STATUS" -eq 0 ]; then
    say "OK"
else
    printf '\033[1;31m[error]\033[0m bridge parity check failed\n' >&2
fi

exit "$STATUS"
