# syntax=docker/dockerfile:1.7

FROM oven/bun:1-alpine AS assets
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile
COPY frontend ./frontend
RUN mkdir -p static && bun run build:assets

FROM rust:1.93-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY migrations ./migrations
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/designstorm /app/server
COPY --from=assets /app/static/app.js /app/static/app.js
COPY static /app/static
COPY docs /app/docs
ENV PORT=8080
EXPOSE 8080
CMD ["/app/server"]
