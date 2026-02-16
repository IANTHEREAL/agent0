FROM --platform=linux/amd64 rust:1.88-bookworm AS builder

# Add arm64 architecture and install cross-compilation toolchain
RUN dpkg --add-architecture arm64 && \
    apt-get update && apt-get install -y \
    cmake \
    protobuf-compiler \
    pkg-config \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    libc6-dev-arm64-cross \
    libssl-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

# Add arm64 target
RUN rustup target add aarch64-unknown-linux-gnu

# Configure cross-compilation
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    OPENSSL_DIR=/usr \
    OPENSSL_INCLUDE_DIR=/usr/include/aarch64-linux-gnu \
    OPENSSL_LIB_DIR=/usr/lib/aarch64-linux-gnu \
    PKG_CONFIG_ALLOW_CROSS=1

WORKDIR /app

# Copy everything needed for build
COPY Cargo.toml Cargo.lock build.rs ./
COPY vendor ./vendor
COPY crates ./crates
COPY src ./src

# Build args for version info (no .git in Docker context)
ARG BUILD_GIT_HASH=""
ARG BUILD_DATE=""
ENV BUILD_GIT_HASH=${BUILD_GIT_HASH}
ENV BUILD_DATE=${BUILD_DATE}

# Cross-compile for arm64
RUN cargo build --release --target aarch64-unknown-linux-gnu

# Runtime stage (arm64)
FROM --platform=linux/arm64 debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/aarch64-unknown-linux-gnu/release/pg-tikv /usr/local/bin/pg-tikv

EXPOSE 5433

CMD ["pg-tikv"]
