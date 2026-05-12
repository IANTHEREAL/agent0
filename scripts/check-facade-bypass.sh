#!/bin/bash
#
# check-facade-bypass.sh — guard against direct TiKV transaction-type imports
# in modules that have been migrated to the storage facade.
#
# Context (#2523, PR-1, Consensus Amendments §B):
#   db9-server is migrating from direct `tikv_client::Transaction` /
#   `TransactionClient` (and later `BoundRange` / `Mutation`) usage to the
#   `crate::storage::facade` types. This script runs in CI and fails when a
#   migrated module reintroduces a direct TiKV-type import. PR-2 (#20) shrinks
#   the allow-list module-by-module; once a module is removed from the
#   allow-list, future direct imports become CI-blocking.
#
# Status in PR-1 (#2524):
#   No call sites have been migrated yet. The allow-list therefore contains
#   every src/ file that currently imports the BLOCK_PATTERNS directly. PR-2
#   removes entries as it migrates them, and the block-list takes effect.
#
# Detection covers two import shapes per blocked symbol S:
#   (a) Qualified usage / direct import:  tikv_client::S<word boundary>
#   (b) Single-line grouped import:       use tikv_client::{...S<word boundary>...}
#
# Known limitations of grep-based detection (per spec §B):
#   1. **Multi-line grouped imports** are NOT detected when the brace block
#      spans multiple lines (the dominant pattern in db9-server today).
#      The script header acknowledges this as a known gap; the follow-up TODO
#      is to replace the grep with a stronger lint (clippy
#      `disallowed-types`, custom rustc lint, or a syn-based scanner).
#   2. `use tikv_client::*` glob imports are NOT inspected. If a module uses
#      a glob import, this script cannot tell which exact symbols are reached.
#   3. Local aliases like `use tikv_client::Transaction as Txn` MAY be missed
#      if the alias does not contain the original symbol name.
#   4. `#[cfg(...)]`-gated imports are inspected the same as unconditional
#      ones; toggling cfg flags does not change the result.
#   5. Re-exports through intermediate modules (e.g. `pub use foo::Transaction`)
#      can launder the type past this guard.
#   These gaps are accepted for the V1 baseline; #2523 amendments mark a
#   follow-up TODO to replace the script with a stronger lint once the
#   migration stabilizes.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

# Symbols that the storage facade absorbs. PR-1 landed the Transaction /
# TransactionClient block. PR-2 adds BoundRange for modules migrated to
# StorageRange; Mutation remains deferred until StorageMutation covers the
# relevant call sites.
BLOCKED_SYMBOLS=(
  'BoundRange'
  'Transaction'
  'TransactionClient'
)

# Allow-list: modules that have NOT yet been migrated to the storage facade.
# Keep entries sorted by path. PR-2 (#20) shrinks this list as call-site
# migration proceeds. New files added to db9-server MUST NOT be added here
# without an accompanying tracking issue and migration plan.
#
# Baseline captured 2026-05-10 from PR-1 audit.
ALLOW_LIST=(
  'src/auth/db9_auth.rs'
  'src/auth/rbac.rs'
  'src/export/lifecycle.rs'
  'src/export/registry.rs'
  'src/extensions/context.rs'
  'src/extensions/embedding.rs'
  'src/extensions/fs/backend.rs'
  'src/extensions/fs/embedded/blob.rs'
  'src/extensions/fs/embedded/bundle.rs'
  'src/extensions/fs/embedded/lifecycle.rs'
  'src/extensions/fs/embedded/mod.rs'
  'src/extensions/fs/embedded/pagefs.rs'
  'src/extensions/fs/stats_worker.rs'
  'src/extensions/http.rs'
  'src/session_context.rs'
  'src/sql/catalog/mod.rs'
  'src/sql/catalog/table_privileges.rs'
  'src/sql/ddl/alter_table/columns.rs'
  'src/sql/ddl/alter_table/constraints.rs'
  'src/sql/ddl/alter_table/mod.rs'
  'src/sql/ddl/create_index.rs'
  'src/sql/ddl/create_table.rs'
  'src/sql/ddl/drop.rs'
  'src/sql/ddl/mod.rs'
  'src/sql/ddl/view.rs'
  'src/sql/ddl_export.rs'
  'src/sql/default_privileges.rs'
  'src/sql/dml/defaults.rs'
  'src/sql/dml/delete.rs'
  'src/sql/dml/foreign_keys/cascade_delete.rs'
  'src/sql/dml/foreign_keys/cascade_update.rs'
  'src/sql/dml/foreign_keys/mod.rs'
  'src/sql/dml/insert.rs'
  'src/sql/dml/update.rs'
  'src/sql/executor/bg_sql.rs'
  'src/sql/executor/core/catalog_prefetch/mod.rs'
  'src/sql/executor/core/catalog_prefetch/resolution.rs'
  'src/sql/executor/core/dispatch/prepared.rs'
  'src/sql/executor/core/mod.rs'
  'src/sql/executor/core/tests.rs'
  'src/sql/executor/core/view_rewrite/expr.rs'
  'src/sql/executor/core/view_rewrite/mod.rs'
  'src/sql/executor/core/view_rewrite/query.rs'
  'src/sql/executor/core/view_rewrite/table.rs'
  'src/sql/executor/cron.rs'
  'src/sql/executor/cte.rs'
  'src/sql/executor/database.rs'
  'src/sql/executor/ddl.rs'
  'src/sql/executor/dml_analyzed/delete.rs'
  'src/sql/executor/dml_analyzed/insert.rs'
  'src/sql/executor/dml_analyzed/mod.rs'
  'src/sql/executor/dml_analyzed/update.rs'
  'src/sql/executor/extensions.rs'
  'src/sql/executor/procedure/materialized_views.rs'
  'src/sql/executor/procedure/procedures.rs'
  'src/sql/executor/select/analyzed/expr_runtime.rs'
  'src/sql/executor/select/analyzed/materialize.rs'
  'src/sql/executor/select/analyzed/materialize_catalog.rs'
  'src/sql/executor/select/analyzed/mod.rs'
  'src/sql/executor/select/analyzed/pipeline.rs'
  'src/sql/executor/select/analyzed/postprocess.rs'
  'src/sql/executor/select/analyzed/pre_materialize.rs'
  'src/sql/executor/select/mod.rs'
  'src/sql/executor/table_functions.rs'
  'src/sql/executor/table_utils/mod.rs'
  'src/sql/executor/udt.rs'
  'src/sql/executor/user_function.rs'
  'src/sql/expr/typed_rewrite.rs'
  'src/sql/hnsw/mod.rs'
  'src/sql/hnsw/storage.rs'
  'src/sql/index_consistency.rs'
  'src/sql/information_schema.rs'
  'src/sql/names.rs'
  'src/sql/operators/context.rs'
  'src/sql/operators/executor.rs'
  'src/sql/plpgsql/executor.rs'
  'src/sql/rbac.rs'
  'src/sql/role_settings.rs'
  'src/sql/runtime_context.rs'
  'src/sql/sequences/ddl.rs'
  'src/sql/sequences/eval.rs'
  'src/sql/sequences/index_helpers.rs'
  'src/sql/sequences/mod.rs'
  'src/sql/sequences/replace.rs'
  'src/sql/session/mod.rs'
  'src/sql/triggers/before.rs'
  'src/sql/triggers/enqueue.rs'
  'src/sql/triggers/execute.rs'
  'src/sql/types/mapping.rs'
  'src/sql/udt/enum_rewrite.rs'
  'src/sql/udt/enum_values.rs'
  'src/sql/udt/mod.rs'
  'src/sql/udt/rename.rs'
  'src/sql/udt/validation.rs'
  'src/storage/facade.rs'
  'src/storage/tikv_store/coprocessor.rs'
  'src/storage/tikv_store/mod.rs'
  'src/txn/mod.rs'
  'src/worker/engine.rs'
  'src/worker/engine/helpers.rs'
  'src/worker/gc/hnsw_impl.rs'
)

# The facade module itself and the TiKV adapter shim must be allowed to use
# these types — they ARE the migration targets / TiKV bridge.
PERMANENT_ALLOW=(
  'src/storage/facade.rs'
  'src/storage/tikv_store/mod.rs'
)

# Build a Bash associative array for O(1) allow-list lookup.
declare -A ALLOWED
for f in "${ALLOW_LIST[@]}" "${PERMANENT_ALLOW[@]}"; do
  ALLOWED["$f"]=1
done

violations=()
for sym in "${BLOCKED_SYMBOLS[@]}"; do
  # Build a PCRE that catches both shapes:
  #   (a) qualified:   tikv_client::Transaction<word boundary>
  #                    (the trailing \b prevents matching `TransactionClient`
  #                    when looking for `Transaction`)
  #   (b) single-line grouped: tikv_client::{...Transaction<word boundary>}
  #
  # Multi-line grouped imports remain a documented limitation (header item 1).
  pattern_qualified='tikv_client::'"${sym}"'\b'
  pattern_grouped='tikv_client::\{[^}]*\b'"${sym}"'\b[^}]*\}'
  combined='('"${pattern_qualified}"'|'"${pattern_grouped}"')'

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    [[ "$file" == src/* ]] || continue
    if [ "${ALLOWED[$file]:-0}" != "1" ]; then
      violations+=("$file uses 'tikv_client::${sym}' but is not on the facade-bypass allow-list")
    fi
  done < <(git grep -lP "$combined" -- 'src/**/*.rs' || true)
done

if [ ${#violations[@]} -gt 0 ]; then
  printf '%s\n' "ERROR: storage-facade bypass detected:" >&2
  for v in "${violations[@]}"; do
    printf '  - %s\n' "$v" >&2
  done
  cat >&2 <<EOF

Migrated modules must use the \`crate::storage::facade\` types instead of
direct \`tikv_client\` storage primitives. Either:

  1. Migrate this file to the facade (preferred), then remove it from the
     ALLOW_LIST in scripts/check-facade-bypass.sh. See #2523 for the
     migration contract.

  2. If migration is genuinely deferred, add the file to ALLOW_LIST with a
     comment naming the tracking issue. New entries SHOULD be paired with a
     follow-up issue and an explicit removal PR.

EOF
  exit 1
fi

echo "OK: storage-facade bypass check passed (allow-list has ${#ALLOW_LIST[@]} entries)."
