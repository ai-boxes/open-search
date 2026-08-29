# syntax=docker/dockerfile:1.7

FROM rust:1.97.1-bookworm AS builder
ENV RUSTUP_TOOLCHAIN=1.97.1 \
    CARGO_PROFILE_RELEASE_STRIP=symbols
WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

ARG TARGETARCH
RUN --mount=type=cache,id=open-search-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=open-search-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=open-search-target-${TARGETARCH},target=/src/target,sharing=locked \
    cargo build --release --locked -p open-search-server \
    && install -Dm755 /src/target/release/open-search /tmp/open-search

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /app --shell /usr/sbin/nologin open-search \
    && mkdir -p /app/data \
    && chown -R open-search:open-search /app

COPY --from=builder /tmp/open-search /usr/local/bin/open-search

USER open-search
ENV LISTEN_ADDRESS=0.0.0.0:8080 \
    DATABASE_PATH=/app/data/open-search.db
EXPOSE 8080
VOLUME ["/app/data"]

HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=5 \
  CMD curl -fsS http://127.0.0.1:8080/healthz >/dev/null

ENTRYPOINT ["open-search"]
CMD ["serve"]
