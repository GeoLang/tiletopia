# Multi-stage build for TileTopia server
ARG MAGO_VERSION=1.16.2
ARG MAGO_SHA256=d37e58b634e91d7ce7ca046168b1db2cf950cd9b2ffacb826fd3b10a8d31f7e2

FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake g++ make libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY patches/ patches/

# Build release binary
RUN cargo build --release --bin tiletopia --features martin

# Minimal runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user for security
RUN useradd -r -s /bin/false tiletopia && \
    mkdir -p /data && chown tiletopia:tiletopia /data

# mago-3d-tiler tiles meshes and vector files, and it needs a JDK 21 runtime
COPY --from=eclipse-temurin:21-jre /opt/java/openjdk /opt/java/openjdk
ENV JAVA_HOME=/opt/java/openjdk
ENV PATH=/opt/java/openjdk/bin:$PATH

ARG MAGO_VERSION
ARG MAGO_SHA256
RUN mkdir -p /opt/mago && \
    curl -fsSL -o /opt/mago/mago-3d-tiler.jar \
      "https://github.com/Gaia3D/mago-3d-tiler/releases/download/v${MAGO_VERSION}/mago-3d-tiler-${MAGO_VERSION}.jar" && \
    echo "${MAGO_SHA256}  /opt/mago/mago-3d-tiler.jar" | sha256sum -c -

COPY --from=builder /app/target/release/tiletopia /usr/local/bin/tiletopia
COPY gui /opt/tiletopia/gui

USER tiletopia

ENV TILETOPIA_MAGO_JAR=/opt/mago/mago-3d-tiler.jar
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
