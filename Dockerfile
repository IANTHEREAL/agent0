# syntax=docker/dockerfile:1
#
# Multi-arch image for db9-server.
#
# This Dockerfile is invoked once per target architecture by the Sys9
# `_sys9-dev-image.yml` reusable workflow, on a runner that natively
# matches the target arch (`arc-runner-amd64-c6i-xl` for linux/amd64,
# `arc-runner-arm64-c7g-xl` for linux/arm64). `$BUILDPLATFORM` therefore
# equals `$TARGETPLATFORM` in every build, so no cross-compilation
# toolchain is needed — cargo builds for the host triple directly.

FROM --platform=$BUILDPLATFORM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        pkg-config \
        libclang-dev \
        libssl-dev \
        libicu-dev \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock build.rs ./
COPY .cargo ./.cargo
COPY vendor ./vendor
COPY crates ./crates
COPY src ./src
COPY proto ./proto

ARG BUILD_GIT_HASH=""
ARG BUILD_DATE=""
# DB9_REQUIRE_PROTOC=1 makes proto-compile failure a hard error so we
# never ship a release image that silently lost the fs9 v2 gRPC backend.
ENV BUILD_GIT_HASH=${BUILD_GIT_HASH} \
    BUILD_DATE=${BUILD_DATE} \
    DB9_REQUIRE_PROTOC=1

# `db9-server` depends on `auth9-core`, which lives in the private
# `db9-ai/db9-auth` repo. Cargo needs HTTPS credentials to clone it
# during dep resolution. The CD workflow forwards the token via the
# `_sys9-dev-image.yml` `secrets.build_secrets` input as
# `gh_token=${{ secrets.CROSS_REPO_TOKEN }}`. Mount it for this single
# RUN; clear the config afterwards so no token survives the layer.
RUN --mount=type=secret,id=gh_token,required=true \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    git config --global url."https://x-access-token:$(cat /run/secrets/gh_token)@github.com/".insteadOf "https://github.com/" \
    && cargo build --release \
    && cp target/release/db9-server /usr/local/bin/db9-server \
    && git config --global --unset url."https://x-access-token:$(cat /run/secrets/gh_token)@github.com/".insteadOf

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 \
        ca-certificates \
        libicu72 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/db9-server /usr/local/bin/db9-server

EXPOSE 5433

CMD ["db9-server"]
