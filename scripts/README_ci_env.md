# CI Artifact Runtime Helper (`scripts/ci_env.sh`)

This helper lets you run a `db9-server` binary downloaded from GitHub Actions CI
on a local machine **without Docker**.

It is designed for the case where CI and local OS library versions differ
(for example CI on Ubuntu 24.04 provides `libicu74`, while your host has `libicu76`).

## What It Does

`scripts/ci_env.sh`:

1. Downloads Ubuntu 24.04 `libicu74` package (`.deb`) to local cache
2. Extracts it under `.cache/ci-env/`
3. Runs your command with `LD_LIBRARY_PATH` prefixed to those extracted libs

It does **not** install or modify system packages.

## Quick Start

From repository root:

```bash
# 1) Prepare local CI-compatible ICU runtime libs (one-time)
bash scripts/ci_env.sh prepare-libs

# 2) Run a CI-downloaded db9-server binary
bash scripts/ci_env.sh run -- /path/to/db9-server --help
```

If you want to export the env manually:

```bash
eval "$(bash scripts/ci_env.sh env)"
/path/to/db9-server --help
```

## Commands

```bash
bash scripts/ci_env.sh prepare-libs
bash scripts/ci_env.sh env
bash scripts/ci_env.sh run -- <command> [args...]
```

## Compatibility Scope

Current script defaults are tuned for CI artifacts built on:

- Ubuntu 24.04 (`libicu74`)
- `x86_64` Linux

Boundaries:

- `arm64` is not auto-handled by default
- Very old Ubuntu releases may still fail due to `glibc` minimum version

## Important: Code-Identical vs Binary-Identical

For PR workflows, GitHub Actions often builds the `pull/<n>/merge` ref, while
local builds are commonly done from `pull/<n>/head`.

These two commits can have:

- identical source tree content (`git diff` empty, same tree hash), but
- different commit IDs/build metadata

So binary files are **not guaranteed** to be byte-identical (`sha256` may differ)
even when code content is functionally the same.

Typical checks:

```bash
# code tree comparison
git rev-parse <head_commit>^{tree}
git rev-parse <merge_commit>^{tree}
git diff --name-status <head_commit> <merge_commit>

# binary comparison
sha256sum /path/to/ci-artifact /path/to/local-build
cmp -s /path/to/ci-artifact /path/to/local-build && echo IDENTICAL || echo DIFFERENT
```

## Required Tools

- `bash`
- `curl`
- `dpkg-deb`

## Cache Location

Default cache root:

```text
.cache/ci-env
```

Override with environment variable:

```bash
DB9_CI_ENV_CACHE_DIR=/your/cache/path
```

## Configuration Overrides

- `DB9_CI_ENV_CACHE_DIR`: cache directory
- `DB9_CI_UBUNTU_MIRROR`: ubuntu mirror base URL
- `DB9_CI_ICU74_DEB`: ICU deb file name

Example:

```bash
DB9_CI_UBUNTU_MIRROR=https://mirrors.ustc.edu.cn/ubuntu \
bash scripts/ci_env.sh prepare-libs
```

## Troubleshooting

1. `libicu*.so.74 not found`
   - Run `bash scripts/ci_env.sh prepare-libs` again
   - Verify with `bash scripts/ci_env.sh env`

2. `missing required command: dpkg-deb`
   - Install `dpkg` package tooling on your machine

3. Binary still fails after ICU fix
   - Check `ldd /path/to/db9-server`
   - You may have additional dependency/version mismatches (for example `glibc`)
