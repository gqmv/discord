# ── Stage 1: install cargo-chef ────────────────────────────────────────────
# cargo-chef separates dependency compilation from source compilation so that
# dependencies are cached in their own layer and only rebuilt when
# Cargo.toml / Cargo.lock actually change.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# ── Stage 2: generate the dependency recipe ─────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: build ──────────────────────────────────────────────────────────
FROM chef AS builder

# Native build-time dependencies:
#   cmake        – required by audiopus_sys (Opus codec used by songbird)
#   libopus-dev  – Opus headers; lets audiopus_sys link dynamically
#   pkg-config   – used by audiopus_sys to locate libopus
#   git          – cargo needs git to fetch the librespot and songbird git deps
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        git \
        libopus-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# 1. Cook dependencies (this layer is cached until Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# 2. Build the application binary
COPY . .
RUN cargo build --release

# ── Stage 4: minimal runtime image ─────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Runtime dependencies:
#   libopus0         – shared library for the Opus codec (audiopus links to it)
#   ca-certificates  – TLS root certificates for rustls-native-roots
RUN apt-get update && apt-get install -y --no-install-recommends \
        libopus0 \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user — Discord bots don't need elevated privileges
RUN useradd -r -u 1001 -s /bin/false botuser

WORKDIR /app
COPY --from=builder /app/target/release/discord-spotify-bot /app/discord-spotify-bot

USER 1001

# OAuth callback server port
EXPOSE 3000

CMD ["/app/discord-spotify-bot"]
