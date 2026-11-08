# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
# Cache dependency compilation: copy manifests first.
COPY Cargo.toml Cargo.lock ./
# Copy real sources.
COPY src ./src
COPY web ./web
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -u 65532 -m bitlab
COPY --from=build /src/target/release/bitlab /usr/local/bin/bitlab
USER 65532:65532
ENV BITLAB_ADDR=:8090 BITLAB_DATA=/data
VOLUME ["/data"]
EXPOSE 8090 51413/tcp 51413/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD wget -qO- http://127.0.0.1:8090/healthz >/dev/null 2>&1 || exit 1
ENTRYPOINT ["/usr/local/bin/bitlab"]
