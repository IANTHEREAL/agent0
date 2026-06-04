#!/usr/bin/env python3
"""
Source-level regression guard for #2567.

This PR fixes a routing bug that is hard to exercise through SQL alone:
db9-server must route fs9 to JuiceFS only, must not keep a process-wide
InitVolume success cache, and must check PD lifecycle before materializing a
missing JuiceFS volume. Keep this in the fast gate so future refactors cannot
silently reintroduce the old embedded fallback path.
"""

from __future__ import annotations

import argparse
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dsn", help="Accepted for regression_gate.py; unused.")
    parser.parse_args()

    backend = read("src/extensions/fs/backend.rs")
    admin = read("src/extensions/fs/grpc/admin.rs")
    auth = read("src/auth/fs_plane_token.rs")
    extensions = read("src/sql/executor/extensions.rs")
    fs_mod = read("src/extensions/fs/mod.rs")
    main_rs = read("src/main.rs")
    table_functions = read("src/sql/executor/table_functions.rs")
    docs = read("docs/fs9_sql_integration.md")
    mint_doc = read("docs/design/fs9_auth9_direct_mint.md")
    proto = read("proto/fsplane/v2/fsplane.proto")

    for forbidden in [
        "FS9_BACKEND",
        "TenantBackendKind",
        "EmbeddedFsBackend::new",
        "failed to init embedded backend",
        "resolve_tenant_backend_kind",
    ]:
        require(
            forbidden not in backend,
            f"backend.rs must not reintroduce legacy fs9 routing symbol {forbidden!r}",
        )

    require(
        "validate_juicefs_lifecycle_state" in backend,
        "backend.rs must keep an explicit JuiceFS lifecycle guard",
    )
    require(
        "expected absent or ENABLED" in backend,
        "lifecycle guard must allow only absent/ENABLED states",
    )
    for teardown_state in ["DISABLED", "ARCHIVED", "TOMBSTONE"]:
        require(
            teardown_state in backend,
            f"lifecycle guard tests must pin teardown state {teardown_state}",
        )

    lifecycle_idx = backend.find("lifecycle_check(")
    materialize_idx = backend.find("ensure_volume(")
    require(lifecycle_idx >= 0, "init path must call lifecycle_check")
    require(materialize_idx >= 0, "init path must call ensure_volume")
    require(
        lifecycle_idx < materialize_idx,
        "lifecycle_check must run before ensure_volume/InitVolume",
    )
    require(
        "pub(crate) async fn ensure_juicefs_lifecycle_allows_init" in backend,
        "lifecycle guard must stay callable by sibling fs9 SQL surfaces",
    )
    require(
        "ensure_fs9_sql_surface_allowed" in backend
        and "is_backend_available()" in backend
        and "is_superuser()" in backend,
        "backend.rs must expose one SQL fs9 gate covering availability, authz, and lifecycle",
    )

    events_guard_idx = table_functions.find("ensure_fs9_sql_surface_allowed")
    events_read_idx = table_functions.find("execute_fs9_events_from_redis")
    require(events_guard_idx >= 0, "fs9_events must call the fs9 SQL gate")
    require(events_read_idx >= 0, "fs9_events must still read the Redis event stream")
    require(
        events_guard_idx < events_read_idx,
        "fs9_events SQL gate must be wired before Redis stream read",
    )
    require(
        extensions.count("ensure_fs9_sql_surface_allowed") >= 2,
        "extensions.fs9 and fs9_jg must both route through the fs9 SQL gate",
    )

    for forbidden in ["volume_init_cache", "volume_is_initialized", "mark_volume_initialized"]:
        require(
            forbidden not in admin,
            f"grpc/admin.rs must not cache InitVolume success with {forbidden}",
        )
    require(
        "Deliberately no process-wide success cache" in admin,
        "grpc/admin.rs must document that InitVolume success is not process-cached",
    )
    require(
        "early fail-closed guard" in admin and "atomic with this remote call" in admin,
        "grpc/admin.rs must document that db9-server's PD read is not atomic with InitVolume",
    )

    require(
        "race-closing lifecycle backstop" in proto,
        "fsplane.proto must document fs9 InitVolume as the lifecycle race backstop",
    )
    require(
        "DISABLED/ARCHIVED/TOMBSTONE" in proto,
        "fsplane.proto must require InitVolume to reject teardown states",
    )

    for forbidden_pd_override in ["FS9_ADMIN_PD_ENDPOINTS", "FS9_GRPC_PD_ENDPOINTS"]:
        require(
            forbidden_pd_override not in admin,
            f"InitVolume meta_url must not use divergent PD env {forbidden_pd_override}",
        )
    require(
        "bind_juicefs_pd_endpoints(pd_endpoints.clone())" in main_rs,
        "main.rs must bind the resolved CLI/env PD endpoints into fs9 runtime state",
    )
    require(
        "JUICEFS_PD_ENDPOINTS_RAW" in fs_mod
        and "juicefs_pd_endpoints_raw()" in admin
        and "juicefs_pd_endpoints()" in backend,
        "lifecycle guard and InitVolume meta_url must share the bound JuiceFS PD endpoint source",
    )

    require(
        "resolve_tenant_backend_kind" not in docs,
        "fs9 docs must not reference deleted resolve_tenant_backend_kind routing",
    )
    require(
        "absence of `jfs_t_`" not in docs,
        "fs9 docs must not say absent jfs_t routes to embedded",
    )
    for forbidden_doc in [
        "silent cross-tenant shadow namespace",
        "fs-plane-only minting",
    ]:
        require(
            forbidden_doc not in docs,
            f"fs9 docs must not retain stale embedded/fs-plane-only text: {forbidden_doc!r}",
        )
    require(
        "not atomic with the remote" in docs,
        "fs9 docs must document the db9-server PD read / InitVolume TOCTOU boundary",
    )
    require(
        "fs9 build whose `InitVolume` handler re-reads" in docs,
        "fs9 docs must document the fs9-side InitVolume lifecycle backstop",
    )
    require(
        "fs-plane-admin" in docs,
        "fs9 docs must document the fs-plane-admin auth9 audience prerequisite",
    )
    require(
        "`fs9_events(...)` does not construct a filesystem backend" in docs
        and "SQL fs9 gate" in docs
        and "superuser authorization" in docs,
        "fs9 docs must document the fs9_events SQL gate",
    )
    require(
        "startup binds the resolved PD endpoint list once" in docs
        and "`FsPlaneAdmin.InitVolume` JuiceFS meta URL" in docs,
        "fs9 docs must document the startup-bound PD endpoint source for guard and InitVolume",
    )

    require(
        'allowed_audiences`. This was previously' in mint_doc
        and '"fs-plane-admin"' in mint_doc,
        "auth9 direct-mint design must require the fs-plane-admin audience",
    )
    require(
        'allowed_audiences = ["fs-plane", "fs-plane-admin"]' in mint_doc,
        "auth9 direct-mint design must cite the production db9-server audience set",
    )
    require(
        "services.db9-server.allowed_audiences" in auth
        and '\\"fs-plane\\"' in auth
        and '\\"fs-plane-admin\\"' in auth,
        "auth9 sign failure must point operators at the full audience allowlist",
    )


if __name__ == "__main__":
    main()
