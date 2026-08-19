# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release \
    && cp target/release/telesms-bot /usr/local/bin/telesms-bot

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /usr/local/bin/telesms-bot /usr/local/bin/telesms-bot
CMD ["telesms-bot"]
