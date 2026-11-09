#!/usr/bin/env bash
# Smoke-test a built local RSSHub engine archive: extract it, boot it through
# the same contract the app uses (PORT + NODE_ENV=production env, health
# polling), fetch a real route and verify it returns RSS XML.
#
# Usage: scripts/smoke-engine.sh [path/to/engine.tar.gz]
#   Without an argument, the newest target/engine/*.tar.gz is used.
#   The route is overridable with ROUTE (default /github/activity/DIYgod).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCHIVE="${1:-$(ls -t "$REPO_ROOT"/target/engine/*.tar.gz 2>/dev/null | head -1 || true)}"
ROUTE="${ROUTE:-/github/activity/DIYgod}"

if [ -z "$ARCHIVE" ] || [ ! -f "$ARCHIVE" ]; then
  echo "no engine archive found — run scripts/build-engine.sh first" >&2
  exit 1
fi
echo "==> smoke-testing $ARCHIVE"

WORK="$(mktemp -d /tmp/papr-engine-smoke.XXXXXX)"
trap 'kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$WORK"' EXIT

# Install the archive exactly like the app does: unpack → <root>/<version>
# layout with engine.json at the root.
mkdir -p "$WORK/root"
tar -xzf "$ARCHIVE" -C "$WORK/root"
cat "$WORK/root/engine.json"
VERSION="$(python3 -c "import json;print(json.load(open('$WORK/root/engine.json'))['version'])")"
ENGINE_DIR="$WORK/root/$VERSION"
NODE_BIN="$ENGINE_DIR/bin/node"
ENTRY=""
for candidate in app/dist/index.js app/dist/index.mjs app/dist/index.cjs; do
  [ -f "$ENGINE_DIR/$candidate" ] && ENTRY="$ENGINE_DIR/$candidate" && break
done
[ -n "$ENTRY" ] || { echo "no entry script found in $ENGINE_DIR" >&2; exit 1; }

# Random loopback port (bind 0, read, drop) — mirrors EngineManager::ensure.
PORT="$(python3 -c "import socket;s=socket.socket();s.bind(('127.0.0.1',0));print(s.getsockname()[1]);s.close()")"

echo "==> starting engine on 127.0.0.1:$PORT (log: $WORK/engine.log)"
(
  cd "$ENGINE_DIR"
  exec env -i \
    PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" \
    PORT="$PORT" NODE_ENV=production NODE_OPTIONS="--max-http-header-size=32768" \
    "$NODE_BIN" "$ENTRY"
) > "$WORK/engine.log" 2>&1 &
SERVER_PID=$!

# Health poll — any HTTP response counts, 60s budget (mirrors the app).
echo "==> waiting for health"
for i in $(seq 1 120); do
  if curl -s -o /dev/null --noproxy '*' "http://127.0.0.1:$PORT/healthz"; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "engine died during startup:" >&2
    tail -20 "$WORK/engine.log" >&2
    exit 1
  fi
  sleep 0.5
  if [ "$i" = 120 ]; then
    echo "engine did not become healthy in 60s:" >&2
    tail -20 "$WORK/engine.log" >&2
    exit 1
  fi
done
echo "==> healthy after ~$((i / 2))s"

# The route under test must answer with RSS XML.
echo "==> GET $ROUTE"
HTTP_CODE="$(curl -s -o "$WORK/route.xml" -w '%{http_code}' --noproxy '*' "http://127.0.0.1:$PORT$ROUTE")"
if [ "$HTTP_CODE" != "200" ]; then
  echo "route returned HTTP $HTTP_CODE:" >&2
  head -20 "$WORK/route.xml" >&2
  exit 1
fi
if ! head -c 4096 "$WORK/route.xml" | grep -qi "<rss\|<feed\|<channel"; then
  echo "route body is not RSS/Atom:" >&2
  head -20 "$WORK/route.xml" >&2
  exit 1
fi
ITEMS="$(grep -c "<item" "$WORK/route.xml" || true)"
echo "==> OK: HTTP 200, RSS XML, $ITEMS item(s)"
echo "==> first title: $(sed -n 's:.*<title>\(.*\)</title>.*:\1:p' "$WORK/route.xml" | sed -n 2p)"
