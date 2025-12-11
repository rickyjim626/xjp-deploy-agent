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

# Install runtime dependencies (git for pull, ca-certs for HTTPS)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    git \
    docker.io \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary
COPY --from=builder /app/target/release/xjp-deploy-agent /usr/local/bin/

# Default port
ENV PORT=9876
EXPOSE 9876

# Run
CMD ["xjp-deploy-agent"]
