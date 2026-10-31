FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p fc-web --bin fcopy-web \
    && strip target/release/fcopy-web

FROM debian:bookworm-slim
ARG VERSION=development
ARG REVISION=unknown
LABEL org.opencontainers.image.title="FCOPY Web" \
      org.opencontainers.image.description="Persistent HTTPS file transfer console" \
      org.opencontainers.image.source="https://github.com/DMG-Workshop/FCOPY" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --user-group --home-dir /config --no-create-home --shell /usr/sbin/nologin fcopy \
    && mkdir /config /data && chown fcopy:fcopy /config /data
COPY --from=build /src/target/release/fcopy-web /usr/local/bin/fcopy-web
ENV HOME=/config
USER 1000:1000
VOLUME ["/config", "/data"]
EXPOSE 8443/tcp
HEALTHCHECK --interval=30s --timeout=6s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error --insecure --max-time 5 https://127.0.0.1:8443/api/info || exit 1
ENTRYPOINT ["/usr/local/bin/fcopy-web"]
