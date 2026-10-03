#!/usr/bin/env bash
# Prod cutover for agente-server on the serving node.
#
# Expects: the repo already fast-forwarded to the release commit. If a freshly
# built binary is staged as ./agente-server.new (scp'd from the build machine —
# the node has no Rust toolchain), it is swapped in first; otherwise the
# current ./agente-server is (re)started. Config comes from ./.env, which
# never leaves the node. Migrations (sqlx, run-once) execute at boot.
#
# Usage, from anywhere:  bash ~/agente.ceo/server/deploy.sh
set -euo pipefail
cd "$(dirname "$0")"

if [ -x agente-server.new ]; then
  mv -f agente-server agente-server.prev 2>/dev/null || true
  mv agente-server.new agente-server
fi
[ -x agente-server ] || { echo "no ./agente-server binary"; exit 1; }

# There may be more than one stray instance (a crashed deploy leaves one
# serving from a renamed inode) — stop them all, one kill per pid.
for pid in $(pgrep -x agente-server || true); do
  kill "$pid" || true
done
for _ in $(seq 20); do
  pgrep -x agente-server >/dev/null || break
  sleep 0.5
done
pgrep -x agente-server >/dev/null && { echo "old process refused to die"; exit 1; }

setsid nohup ./agente-server >> server.log 2>&1 < /dev/null &

# Probe wherever this deployment actually binds (.env BIND_ADDR, default 8118).
addr=$(sed -n 's/^BIND_ADDR=//p' .env 2>/dev/null | tail -1)
addr=${addr:-127.0.0.1:8118}
addr=${addr/0.0.0.0/127.0.0.1}

for _ in $(seq 30); do
  if curl -sf -m 2 "http://${addr}/health" >/dev/null; then
    echo "deployed: $(git log --oneline -1)"
    exit 0
  fi
  sleep 1
done

echo "server did not become healthy — check server.log; previous binary kept as ./agente-server.prev"
exit 1
