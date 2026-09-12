# The gateway, built once and run anywhere.
#
# Two stages: a builder with the Rust toolchain, and a runtime image that
# carries the binary and nothing else. The result is a few megabytes on top of
# Debian slim, needs no toolchain on the server, and keeps every piece of
# state in /data — so moving the whole service to another machine is copying
# one directory and starting the container again.
FROM rust:1-bookworm AS build

WORKDIR /src

# The workspace as a whole: the gateway shares two crates with the desktop,
# so a copy of the gateway alone would not compile.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY server-gateway ./server-gateway
# Cargo insists on every workspace member existing. The desktop is not built
# here — it needs a webview toolchain the server has no use for — so it is
# replaced by an empty crate of the same name.
RUN sed -i 's#, "src-tauri"##' Cargo.toml

RUN cargo build --release --locked -p velvetdesk-gateway

FROM debian:bookworm-slim AS runtime

# TLS roots, because every upstream is https. Nothing else is needed: the
# binary is static apart from libc, and sqlite is compiled into it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# An unprivileged user that owns the data directory, so a container that is
# somehow broken into is not root on the volume.
RUN useradd --system --uid 10001 --home /data velvetdesk \
    && mkdir -p /data \
    && chown velvetdesk:velvetdesk /data

COPY --from=build /src/target/release/velvetdesk-gateway /usr/local/bin/velvetdesk-gateway

USER velvetdesk
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8787

# Everything the gateway keeps lives under the volume: the database, and the
# config file that seeds it on a first run.
ENV VD_GATEWAY_CONFIG=/data/gateway.json

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD ["/usr/local/bin/velvetdesk-gateway", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/velvetdesk-gateway"]
CMD ["serve"]
