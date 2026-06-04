use anyhow::Result;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

use crate::model::{Row, TableSchema};
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod channel_reader;
pub(crate) mod config;
pub(crate) mod decoders;
pub(crate) mod embedded;
pub(crate) mod glob;
pub(crate) mod grpc;
pub(crate) mod normalizing;
pub(crate) mod notify;
pub(crate) mod redis_events;
pub(crate) mod s3;
pub(crate) mod sql_client;
pub(crate) mod stats_worker;
pub(crate) mod streaming;
pub(crate) mod termination_guard;
pub(crate) mod upload_token;
pub(crate) mod ws;

mod directory;
mod file_stream;
mod glob_stream;
mod table_function;

pub(crate) enum Fs9Mode {
    Directory {
        path: String,
        recursive: bool,
        exclude: Option<String>,
    },
    File {
        path: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
    },
    Glob {
        pattern: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
        exclude: Option<String>,
    },
}

pub(crate) const MAX_BYTES_PER_FILE: usize = 100 * 1024 * 1024;
pub(crate) const MAX_FILES_PER_GLOB: usize = 10_000;
pub(crate) const MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;
pub(crate) const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";
static JUICEFS_PD_ENDPOINTS_RAW: OnceLock<String> = OnceLock::new();

/// Per-call ceiling for in-place offset writes (`fs9_write_at`). fs9 v2
/// `WriteAt` is a unary RPC with no multipart variant, so any single
/// call must fit a gRPC message. Aligned to fs9's chunk-size convention
/// (`PutFile` unary cutoff is the same value) — bulk writes belong on
/// `fs9_write` / `fs9_append`, which transparently fall back to
/// multipart for larger payloads.
pub(crate) const MAX_BYTES_PER_OFFSET_WRITE: usize = 4 * 1024 * 1024;

/// JuiceFS volume name for a given db9 tenant. Used in fs-plane JWT
/// `scp` claims, in the gRPC `volume_id` field, and as the PD keyspace
/// name fs9 manages.
pub(crate) fn jfs_volume_id(tenant_id: &str) -> String {
    format!("jfs_t_{tenant_id}")
}

/// Canonicalize a raw PD endpoint string into the one normalized form every
/// JuiceFS consumer must share: each segment trimmed, empty segments dropped,
/// rejoined with `,` (falling back to the default when nothing remains).
///
/// This is what makes "one resolved PD source" actually hold end to end: the
/// lifecycle guard (which parses the list) and `InitVolume.meta_url` (which
/// interpolates the string verbatim into `tikv://{pd}?...`) both resolve to an
/// identical endpoint set, regardless of incidental whitespace or empty
/// segments in the operator-provided value (e.g. `"pd1:2379, pd2:2379"`).
pub(crate) fn canonicalize_juicefs_pd_endpoints(raw: &str) -> String {
    let joined = parse_juicefs_pd_endpoints(raw).join(",");
    if joined.is_empty() {
        DEFAULT_PD_ENDPOINTS.to_string()
    } else {
        joined
    }
}

/// Bind the resolved process-wide PD endpoint list used by JuiceFS volume
/// lifecycle checks and lazy InitVolume meta URLs. The value is canonicalized
/// once here so every downstream consumer reads an identical endpoint set.
pub(crate) fn bind_juicefs_pd_endpoints(raw: String) {
    let normalized = canonicalize_juicefs_pd_endpoints(&raw);
    if let Err(existing) = JUICEFS_PD_ENDPOINTS_RAW.set(normalized.clone()) {
        if existing != normalized {
            tracing::warn!(
                existing,
                requested = normalized,
                "ignoring second JuiceFS PD endpoint binding"
            );
        }
    }
}

/// Single runtime source of truth for the PD endpoint list used by JuiceFS
/// lifecycle checks and lazy InitVolume meta URLs. Always canonical: the bound
/// value is canonicalized at bind time, and the env/default fallback (used
/// before `bind_juicefs_pd_endpoints`, e.g. in tests) is canonicalized here.
pub(crate) fn juicefs_pd_endpoints_raw() -> String {
    if let Some(bound) = JUICEFS_PD_ENDPOINTS_RAW.get() {
        return bound.clone();
    }
    canonicalize_juicefs_pd_endpoints(
        &crate::config::env_string("PD_ENDPOINTS").unwrap_or_default(),
    )
}

pub(crate) fn parse_juicefs_pd_endpoints(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub(crate) fn juicefs_pd_endpoints() -> Vec<String> {
    parse_juicefs_pd_endpoints(&juicefs_pd_endpoints_raw())
}

/// Reject mutating operations whose target path normalizes to the
/// filesystem root. Both `EmbeddedFsBackend` and `GrpcFsBackend` route
/// `remove` / `remove_recursive` / `rename` through this helper so a
/// JuiceFS tenant sees the same `PermissionDenied("cannot {op} root")`
/// as an embedded tenant, before any IO.
///
/// The recursive grpc walker stats/readdirs its target and deletes
/// children client-side before issuing the final delete on the target
/// itself, so without an up-front root guard `fs9_remove('/', true)`
/// would unlink every top-level entry on a JuiceFS volume before
/// failing on the root — a destructive contract mismatch with the
/// embedded backend, which refuses root before traversal.
///
/// Path-collapse semantics deliberately match fs9's server-side
/// `path.Clean` (juicedata/juicefs `pkg/fs/fs.go` `doResolve` opens
/// with `p = path.Clean(p)`): both empty segments and `.` segments
/// are eliminated before the equality check. `/`, `//`, `///`, `""`,
/// `/.`, `/./`, `/.//.` all collapse to root and are rejected; any
/// segment that is neither empty nor `.` keeps the path non-root.
/// `..` segments are validated server-side (fs9's `validatePath`
/// rejects them), so the helper does not need to model them.
///
/// Why match fs9 semantics rather than embedded `normalize_path`:
/// the gRPC walker calls `stat` / `readdir` / `remove` on the literal
/// caller path, which fs9 resolves through `path.Clean`. If the
/// client-side guard followed embedded's stricter "only empty
/// segments" rule, `/.` would pass the guard, alias root on fs9, and
/// recursively unlink every top-level entry before the final root
/// delete failed. Embedded backend's resolver returns NotFound on
/// `/.` today (safe by accident); routing both backends through this
/// helper converts that to an honest `PermissionDenied`.
pub(crate) fn reject_root_path_op(path: &str, op: &str) -> Result<()> {
    if path.split('/').all(|seg| seg.is_empty() || seg == ".") {
        return Err(anyhow::anyhow!(
            crate::extensions::fs::embedded::types::EmbeddedFsError::PermissionDenied(format!(
                "cannot {op} root"
            ))
        ));
    }
    Ok(())
}

/// Canonicalize a caller-supplied path to the form fs9 v2's
/// `path.Clean` produces. Single source of truth for "what shape does
/// the FsBackend contract require of its inputs", routed through every
/// path-bearing trait method by `NormalizingFsBackend`.
///
/// Backends disagree about path interpretation in two ways:
///
/// 1. **Absolute prefix.** `EmbeddedFsBackend` happens to be tolerant
///    because `pagefs::normalize_path` re-prepends `/`; `GrpcFsBackend`
///    forwards raw and fs9 v2 `validatePath` rejects non-absolute
///    paths. This was PR #2547's first review finding.
/// 2. **Dot segments.** `pagefs::resolve_path` walks each segment via
///    `lookup(parent_inode, segment)`, treating `.` as a literal child
///    name; fs9 v2 server-side `path.Clean` collapses `.` and rejects
///    `..`. Routing through the same canonical form makes both
///    backends operate on the same target. PR #2547's second review
///    finding caught this for `/./foo`, `/foo/.`, `/foo/./bar` on
///    mutating ops — on JuiceFS those alias `/foo`, on embedded they
///    create a child literally named `.`.
///
/// Rules applied (equivalent to fs9 v2 server-side `path.Clean`):
/// - empty input → `/`
/// - non-absolute input → prepend `/`
/// - drop empty segments (`/a//b` → `/a/b`, `/a/` → `/a`)
/// - drop `.` segments (`/a/./b` → `/a/b`, `/./` → `/`)
/// - reject `..` segments with `InvalidInput`
///
/// Dotfile names (segments that START with `.` but are not exactly `.`
/// or `..`) are preserved — `/.hidden` and `/foo/.gitignore` are valid
/// inputs and must reach the backend intact.
///
/// `Err` arm carries `EmbeddedFsError::InvalidInput`, which both
/// backends emit uniformly via the shared error type (see
/// `grpc::errors::fs_error_to_anyhow` mapping `FsErrorInvalidArgument`
/// to the same variant). Caller-visible behavior: `..` segments
/// produce the same SQLSTATE on embedded and JuiceFS tenants.
pub(crate) fn to_fs9_canonical_path(path: &str) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                return Err(anyhow::anyhow!(
                    crate::extensions::fs::embedded::types::EmbeddedFsError::InvalidInput(format!(
                        "path must not contain '..' segments: {path:?}"
                    ),)
                ));
            }
            _ => parts.push(seg),
        }
    }
    if parts.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", parts.join("/")))
    }
}

pub(crate) use directory::list_directory_entries;
pub(crate) use file_stream::start_file_stream;
pub(crate) use glob_stream::start_glob_stream;
pub(crate) use table_function::{execute_table_function, infer_table_function_schema};

#[cfg(test)]
pub(crate) use file_stream::start_file_stream_for_test_backend;
#[cfg(test)]
pub(crate) use glob_stream::start_glob_stream_with_budget_for_test_backend;
#[cfg(test)]
pub(crate) use table_function::execute_table_function_with_budget_for_test_backend;

#[cfg(test)]
mod tests;
