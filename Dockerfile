FROM rust:1.83-bookworm

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    protobuf-compiler \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock* ./

# Create a dummy main.rs to cache dependencies
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# Build dependencies (this layer will be cached)
RUN cargo build --release 2>/dev/null || cargo build 2>/dev/null || true

# Remove dummy source
RUN rm -rf src

# Copy actual source code
COPY src ./src

# Build the actual application
RUN cargo build --release

EXPOSE 5432

CMD ["./target/release/pg-tikv"]
