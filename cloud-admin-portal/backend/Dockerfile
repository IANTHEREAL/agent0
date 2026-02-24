# Cross-compile from x86_64 to arm64 (EKS nodes are arm64)
FROM --platform=linux/amd64 rust:1.85-bookworm AS builder

RUN dpkg --add-architecture arm64 && \
    apt-get update && apt-get install -y \
    gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
    libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add aarch64-unknown-linux-gnu

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++

ARG BUILD_GIT_HASH=unknown

WORKDIR /app

COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/

ENV BUILD_GIT_HASH=${BUILD_GIT_HASH}
RUN cargo build --release --target aarch64-unknown-linux-gnu

FROM --platform=linux/arm64 debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/aarch64-unknown-linux-gnu/release/db9-admin /usr/local/bin/db9-admin
COPY --from=builder /app/target/aarch64-unknown-linux-gnu/release/db9-ctl /usr/local/bin/db9-ctl
COPY --from=builder /app/target/aarch64-unknown-linux-gnu/release/db9 /usr/local/bin/db9

EXPOSE 8090

CMD ["db9-admin"]
