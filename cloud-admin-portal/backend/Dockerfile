FROM rust:1.83-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock* ./
COPY src/ src/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/pgtikv-admin /usr/local/bin/pgtikv-admin
COPY --from=builder /app/target/release/pgtikv-ctl /usr/local/bin/pgtikv-ctl

EXPOSE 8090

CMD ["pgtikv-admin"]
