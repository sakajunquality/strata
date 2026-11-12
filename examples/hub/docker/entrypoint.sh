#!/bin/sh
# Starts the hub; with HUB_DB_REPLICA set, under Litestream.
set -eu

if [ -z "${HUB_DB_REPLICA:-}" ]; then
  exec bun run server/index.ts
fi

mkdir -p "$(dirname "$HUB_DB")"
# No file here yet (a new instance): fetch the latest copy. No copy there yet
# (the very first start): go on with an empty database.
litestream restore -config /etc/litestream.yml -if-db-not-exists -if-replica-exists "$HUB_DB"
# Litestream stays in front: it passes the signal to stop on to the hub and
# copies what is left before it exits.
exec litestream replicate -config /etc/litestream.yml -exec "bun run server/index.ts"
