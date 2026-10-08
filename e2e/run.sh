#!/usr/bin/env bash
# The landing page in headless Chromium, against real servers: a nostr
# sign-up signed by a fake NIP-07 signer (real Schnorr signatures), the npub
# fallback, then approval and provisioning into a local bare git repository.
# Needs `npm ci --prefix e2e` and `npx --prefix e2e playwright install chromium`.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet
bin=$PWD/target/debug/lnurlcash-run
work=$(mktemp -d "${TMPDIR:-/tmp}/lnurlcash-run-e2e.XXXX")
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

serve() { # port, data dir, extra env...
  local port=$1 data=$2
  shift 2
  env PUBLIC_URL="http://127.0.0.1:$port" LISTEN="127.0.0.1:$port" DATA_DIR="$data" "$@" "$bin" >"$data.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do curl -sf "http://127.0.0.1:$port/healthz" >/dev/null && return; sleep 0.1; done
  echo "server on $port did not come up:" >&2; cat "$data.log" >&2; exit 1
}

mkdir -p "$work/shots"
serve 8291 "$work/landing"
node e2e/landing.mjs http://127.0.0.1:8291/ "$work/shots"

git init -q --bare -b main "$work/remote.git"
serve 8292 "$work/provision" GITOPS_REPO="$work/remote.git"
node e2e/provision.mjs http://127.0.0.1:8292/ "$work/shots" "$bin" "$work/provision" "$work/remote.git"

if [ -n "${SHOTS:-}" ]; then mkdir -p "$SHOTS" && cp "$work"/shots/*.png "$SHOTS"/; fi
echo "e2e: PASS"
