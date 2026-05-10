# Multi-stage build for TileTopia server
FROM rust:1.87-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Build release binary (--no-default-features avoids draco-rs compilation issues)
RUN cargo build --release --no-default-features --bin tiletopia

# Minimal runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user for security
RUN useradd -r -s /bin/false tiletopia

COPY --from=builder /app/target/release/tiletopia /usr/local/bin/tiletopia
COPY gui /opt/tiletopia/gui

USER tiletopia

ENV TILETOPIA_DATA_DIR=/data
ENV TILETOPIA_PORT=3000
ENV TILETOPIA_GUI_DIR=/opt/tiletopia/gui
ENV RUST_LOG=info,tiletopia=debug

EXPOSE 3000
VOLUME /data

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:3000/api/v1/health || exit 1

ENTRYPOINT ["tiletopia"]
CMD ["serve", "--data-dir", "/data", "--host", "0.0.0.0", "--port", "3000"]
