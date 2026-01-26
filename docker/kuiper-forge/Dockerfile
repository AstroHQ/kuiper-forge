FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

ARG DB_BACKEND=postgres

WORKDIR /src

# Copy workspace manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY kuiper-forge/Cargo.toml kuiper-forge/Cargo.toml
COPY kuiper-agent-proto/Cargo.toml kuiper-agent-proto/Cargo.toml
COPY kuiper-agent-lib/Cargo.toml kuiper-agent-lib/Cargo.toml
COPY kuiper-tart-agent/Cargo.toml kuiper-tart-agent/Cargo.toml
COPY kuiper-proxmox-agent/Cargo.toml kuiper-proxmox-agent/Cargo.toml
COPY kuiper-proxmox-api/Cargo.toml kuiper-proxmox-api/Cargo.toml

# Create dummy source files so cargo can resolve the workspace and cache dependencies
RUN mkdir -p kuiper-forge/src && echo "fn main() {}" > kuiper-forge/src/main.rs && touch kuiper-forge/src/lib.rs \
    && mkdir -p kuiper-agent-proto/src && touch kuiper-agent-proto/src/lib.rs \
    && mkdir -p kuiper-agent-proto/proto && touch kuiper-agent-proto/proto/agent.proto && touch kuiper-agent-proto/proto/management.proto \
    && echo 'fn main() {}' > kuiper-agent-proto/build.rs \
    && mkdir -p kuiper-agent-lib/src && touch kuiper-agent-lib/src/lib.rs \
    && mkdir -p kuiper-tart-agent/src && echo "fn main() {}" > kuiper-tart-agent/src/main.rs \
    && mkdir -p kuiper-proxmox-agent/src && echo "fn main() {}" > kuiper-proxmox-agent/src/main.rs \
    && mkdir -p kuiper-proxmox-api/src && touch kuiper-proxmox-api/src/lib.rs

RUN cargo build --release -p kuiper-forge --no-default-features --features ${DB_BACKEND} || true

# Copy real source and rebuild
COPY . .
# Touch source files to invalidate the dummy build cache
RUN touch kuiper-forge/src/main.rs kuiper-agent-proto/build.rs kuiper-agent-proto/src/lib.rs

RUN cargo build --release -p kuiper-forge --no-default-features --features ${DB_BACKEND}

# --- Runtime ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

RUN groupadd --system kuiper && useradd --system --gid kuiper --create-home kuiper

COPY --from=builder /src/target/release/kuiper-forge /usr/local/bin/kuiper-forge
COPY docker/kuiper-forge/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

RUN mkdir -p /data && chown kuiper:kuiper /data

USER kuiper

# TLS paths default to /data (entrypoint passes --data-dir /data to the binary)
ENV KUIPER_TLS__CA_CERT=/data/ca.crt
ENV KUIPER_TLS__CA_KEY=/data/ca.key
ENV KUIPER_TLS__SERVER_CERT=/data/server.crt
ENV KUIPER_TLS__SERVER_KEY=/data/server.key
# GitHub App private key: provide ONE of these in your compose/env:
#   KUIPER_GITHUB__PRIVATE_KEY_PATH=/data/github-app.pem  (file mount)
#   KUIPER_GITHUB__PRIVATE_KEY="-----BEGIN RSA..."         (inline PEM, supports \n escapes)
ENV KUIPER_GRPC__LISTEN_ADDR=0.0.0.0:9443
ENV KUIPER_NO_LOG_FILE=true

VOLUME /data
EXPOSE 9443

ENTRYPOINT ["entrypoint.sh"]
CMD ["kuiper-forge", "serve"]
