# The hub as a container image. The build context is this directory:
#
#   docker build -t strata-hub examples/hub
#
# It listens on 0.0.0.0:8080 and keeps its SQLite file at /data/hub.sqlite.
# With HUB_DB_REPLICA set (gs://bucket/path, s3://bucket/path), Litestream
# restores that file from there on start and copies every change back, which
# is what makes the hub runnable where a container's disk does not outlive it.
FROM oven/bun:1 AS build
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile
COPY tsconfig.json vite.config.ts ./
COPY web web
COPY server server
RUN bun run build
# What the server needs at run time, without the UI's build tools.
RUN rm -rf node_modules && bun install --frozen-lockfile --production

FROM litestream/litestream@sha256:0ba1cdbb2d0be8876e27b1e646dc031309b7e3928e0ed91503ebaa73ff7e2c5d AS litestream

FROM oven/bun:1-slim
# git fetches mirrors; certificates are for Strata, sign-in providers and webhooks over TLS.
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/* \
    && mkdir /data && chown bun:bun /data
COPY --from=litestream /usr/local/bin/litestream /usr/local/bin/litestream
WORKDIR /app
COPY --from=build /app/node_modules node_modules
COPY --from=build /app/dist dist
COPY package.json ./
COPY server server
COPY docker/litestream.yml /etc/litestream.yml
COPY docker/entrypoint.sh /usr/local/bin/hub-entrypoint
RUN chmod 0755 /usr/local/bin/hub-entrypoint
USER bun
ENV HUB_HOST=0.0.0.0 HUB_PORT=8080 HUB_DB=/data/hub.sqlite HUB_MIRROR_DIR=/data/mirrors
EXPOSE 8080
ENTRYPOINT ["hub-entrypoint"]
