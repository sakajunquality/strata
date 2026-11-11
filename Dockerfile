# The strata server as a container image.
#
#   docker build -t strata .
#
# The build is cached between runs (registry and target directory), so a
# second build compiles only what changed.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p strata-server && cp target/release/strata /strata

FROM debian:bookworm-slim
# Certificates for object storage and managed databases over TLS.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /strata /usr/local/bin/strata
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["strata"]
CMD ["--listen", "0.0.0.0:8080"]
