#!/usr/bin/env bash
# Build a self-contained local RSSHub engine archive for papr.
#
# The engine is a directory layout the app's EngineManager understands:
#
#   engine.json                    { version, rsshubRevision, nodeVersion }
#   bin/node                       the bundled Node runtime (node.exe on Windows)
#   app/                           the RSSHub build product
#     package.json  dist/  node_modules/   (production deps only)
#
# Browser-automation packages (patchright / playwright / puppeteer) are never
# installed into the product: routes that need them are marked unsupported in
# the marketplace. sharp keeps its prebuilt platform binaries via a plain
# production install on the build platform.
#
# Usage: scripts/build-engine.sh [--fresh]
#   --fresh   wipe the RSSHub clone / node_modules cache and rebuild from master
#
# Outputs into target/engine/ (gitignored):
#   rsshub-engine-<platform>-<node>-<rev>.tar.gz + .sha256
#
# Heavy artifacts (the RSSHub clone, pnpm store, Node distribution) are cached
# under target/engine-cache/ so repeated runs are cheap.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Prefer the system pnpm (CI installs it via pnpm/action-setup); fall back to
# corepack, which fetches the version RSSHub's packageManager field pins.
if command -v pnpm >/dev/null 2>&1; then
  PNPM=(pnpm)
else
  PNPM=(corepack pnpm)
fi
CACHE="$REPO_ROOT/target/engine-cache"
OUT="$REPO_ROOT/target/engine"
FRESH="${1:-}"

mkdir -p "$CACHE" "$OUT"

# ── platform ─────────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin) os="darwin" ;;
  Linux) os="linux" ;;
  MINGW*|MSYS*|CYGWIN*) os="win" ;;
  *) echo "unsupported OS: $OS" >&2; exit 1 ;;
esac
case "$ARCH" in
  arm64|aarch64) arch="arm64" ;;
  x86_64) arch="x64" ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
PLATFORM="${os}-${arch}"

# ── Node version ─────────────────────────────────────────────────────────
# RSSHub master declares engines.node ^24.15.0 || ^26.13.0 — stay on the 24
# LTS line at or above the floor.
NODE_MAJOR="24"
NODE_VERSION="v24.22.2"

# ── RSSHub source (shallow clone, cached) ─────────────────────────────────
RSSHUB="$CACHE/rsshub"
if [ "$FRESH" = "--fresh" ]; then
  rm -rf "$RSSHUB"
fi
if [ ! -d "$RSSHUB/.git" ]; then
  echo "==> cloning RSSHub (shallow)"
  git clone --depth 1 https://github.com/DIYgod/RSSHub "$RSSHUB"
fi
RSSHUB_REV="$(git -C "$RSSHUB" rev-parse HEAD)"
echo "==> RSSHub at $RSSHUB_REV"

# ── build (cached: skip when the same revision already built) ──────────────
BUILD_MARKER="$CACHE/.built-$RSSHUB_REV"
if [ "$FRESH" = "--fresh" ] || [ ! -f "$BUILD_MARKER" ]; then
  cd "$RSSHUB"
  echo "==> pnpm install (skipping browser downloads)"
  export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=true
  export PUPPETEER_SKIP_DOWNLOAD=true
  export PUPPETEER_SKIP_CHROMIUM_DOWNLOAD=true
  ${PNPM[@]} install --frozen-lockfile
  echo "==> pnpm build"
  ${PNPM[@]} run build
  touch "$BUILD_MARKER"
  cd "$REPO_ROOT"
else
  echo "==> reusing cached build for $RSSHUB_REV"
fi

# Locate the built entry (tsdown emits dist/index.js for lib/index.ts).
ENTRY=""
for candidate in dist/index.js dist/index.mjs dist/index.cjs; do
  if [ -f "$RSSHUB/$candidate" ]; then
    ENTRY="$candidate"
    break
  fi
done
if [ -z "$ENTRY" ]; then
  echo "could not find the RSSHub build entry under $RSSHUB/dist" >&2
  ls "$RSSHUB" >&2 || true
  exit 1
fi
echo "==> entry: $ENTRY"

# ── Node runtime (cached download) ────────────────────────────────────────
NODE_DIST="$CACHE/node-$NODE_VERSION-$PLATFORM"
NODE_URL="https://nodejs.org/dist/$NODE_VERSION/node-$NODE_VERSION-$PLATFORM.tar.gz"
if [ "$os" = "win" ]; then
  NODE_URL="https://nodejs.org/dist/$NODE_VERSION/node-$NODE_VERSION-$PLATFORM.zip"
fi
if [ ! -d "$NODE_DIST" ]; then
  echo "==> downloading Node $NODE_VERSION for $PLATFORM"
  mkdir -p "$NODE_DIST.dl"
  # nodejs.org first; the npmmirror CDN is the fallback for networks where
  # nodejs.org is unreachable.
  if ! curl -fsSL "$NODE_URL" -o "$NODE_DIST.dl/archive"; then
    MIRROR_URL="https://npmmirror.com/mirrors/node/$NODE_VERSION/node-$NODE_VERSION-$PLATFORM.tar.gz"
    if [ "$os" = "win" ]; then
      MIRROR_URL="https://npmmirror.com/mirrors/node/$NODE_VERSION/node-$NODE_VERSION-$PLATFORM.zip"
    fi
    echo "==> nodejs.org unreachable, falling back to npmmirror"
    curl -fsSL "$MIRROR_URL" -o "$NODE_DIST.dl/archive"
  fi
  tar -xzf "$NODE_DIST.dl/archive" -C "$NODE_DIST.dl"
  mv "$NODE_DIST.dl/node-$NODE_VERSION-$PLATFORM" "$NODE_DIST"
  rm -rf "$NODE_DIST.dl"
fi
NODE_BIN="$NODE_DIST/bin/node"
if [ "$os" = "win" ]; then
  NODE_BIN="$NODE_DIST/node.exe"
fi
if [ ! -f "$NODE_BIN" ]; then
  # The Windows zip has no top-level directory flattening above; search once.
  NODE_BIN="$(find "$NODE_DIST" -name 'node.exe' -o -name node | head -1)"
fi
if [ ! -f "$NODE_BIN" ]; then
  echo "node binary not found in $NODE_DIST" >&2
  exit 1
fi

# ── assemble the engine tree ──────────────────────────────────────────────
STAGING="$OUT/staging-$PLATFORM"
rm -rf "$STAGING"
mkdir -p "$STAGING/bin" "$STAGING/app"

echo "==> staging production dependencies"
cp "$RSSHUB/package.json" "$RSSHUB/pnpm-lock.yaml" "$STAGING/app/" 2>/dev/null \
  || cp "$RSSHUB/package.json" "$STAGING/app/"
# pnpm-workspace.yaml carries RSSHub's install settings (notably
# blockExoticSubdeps: false — the lockfile contains one git-hosted dep);
# without it the filtered prod install refuses to resolve.
[ -f "$RSSHUB/pnpm-workspace.yaml" ] && cp "$RSSHUB/pnpm-workspace.yaml" "$STAGING/app/"
# patchedDependencies (package.json → pnpm.patchedDependencies) points at
# patches/<pkg>.patch — the install fails without the patch files.
[ -d "$RSSHUB/patches" ] && cp -R "$RSSHUB/patches" "$STAGING/app/patches"
# Copy the built output into staging/app/<entry-dir>. cp into the *parent* —
# pre-creating the destination dir would nest it (app/dist/dist).
cp -R "$RSSHUB/$(dirname "$ENTRY")" "$STAGING/app/"
(
  cd "$STAGING/app"
  # Production install into the staging copy: pulls only runtime deps with
  # the right prebuilt binaries for this platform (sharp included) and no
  # browser-automation packages, which are dev dependencies of RSSHub.
  # --no-frozen-lockfile: with --prod, pnpm refilters the lockfile and trips
  # over RSSHub's `overrides` when it is frozen; the staging copy is
  # disposable, so unfrozen resolution (still lockfile-seeded) is fine here.
  export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=true
  export PUPPETEER_SKIP_DOWNLOAD=true
  export PUPPETEER_SKIP_CHROMIUM_DOWNLOAD=true
  ${PNPM[@]} install --prod --no-frozen-lockfile
  # Belt and braces: no browser automation may ship, even if the lockfile
  # ever reclassifies it.
  rm -rf node_modules/patchright node_modules/playwright node_modules/playwright-core \
        node_modules/puppeteer node_modules/puppeteer-core node_modules/@puppeteer 2>/dev/null || true
)

echo "==> bundling node"
cp "$NODE_BIN" "$STAGING/bin/node"
[ "$os" = "win" ] && mv "$STAGING/bin/node" "$STAGING/bin/node.exe"

cat > "$STAGING/engine.json" <<JSON
{
  "version": "1",
  "rsshubRevision": "$RSSHUB_REV",
  "nodeVersion": "$NODE_VERSION"
}
JSON

# ── archive ───────────────────────────────────────────────────────────────
ARCHIVE="$OUT/rsshub-engine-$PLATFORM-$NODE_VERSION-${RSSHUB_REV:0:12}.tar.gz"
echo "==> packing $ARCHIVE"
tar -czf "$ARCHIVE" -C "$STAGING" .
SHASUM="$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
printf '%s  %s\n' "$SHASUM" "$(basename "$ARCHIVE")" > "$ARCHIVE.sha256"
rm -rf "$STAGING"

echo "==> done: $ARCHIVE"
echo "    sha256: $SHASUM"
