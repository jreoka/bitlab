# Multi-stage build to keep the runner image small and clean
FROM rust:1.85-slim-bookworm AS builder

WORKDIR /usr/src/app

# Install build-time dependencies (required for compiling dependencies like openssl)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    g++ \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies: Create a dummy main.rs and build dependencies first
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Copy actual source files and build the production binary
COPY src ./src
COPY assets ./assets
RUN touch src/main.rs
RUN cargo build --release

# Runner stage
FROM debian:bookworm-slim

WORKDIR /app

# Install run-time dependencies (ca-certificates for external API requests, openssl for SSL support)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    openssl \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/bitlab /app/bitlab

# Run the binary
CMD ["./bitlab"]
