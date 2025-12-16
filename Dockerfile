# Build stage
FROM rust:1.83-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy source
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies (git for pull, docker for containers, ca-certs for HTTPS)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    git \
    docker.io \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install docker-compose v2 standalone (compatible with newer Docker API)
RUN COMPOSE_VERSION=$(curl -s https://api.github.com/repos/docker/compose/releases/latest | grep tag_name | cut -d '"' -f 4) \
    && curl -SL "https://github.com/docker/compose/releases/download/${COMPOSE_VERSION}/docker-compose-linux-x86_64" -o /usr/local/bin/docker-compose \
    && chmod +x /usr/local/bin/docker-compose

WORKDIR /app

# Copy binary
COPY --from=builder /app/target/release/xjp-deploy-agent /usr/local/bin/

# Default port
ENV PORT=9876
EXPOSE 9876

# Run
CMD ["xjp-deploy-agent"]
