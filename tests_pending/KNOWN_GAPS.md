# Known Gaps — tests_pending/ Manifest

These 81 tests document SQL features not yet supported by db9-server.
They remain in `tests_pending/` until the underlying gap is resolved, at which
point they should be promoted to `tests/` and added to `regression_gate.list`.

Audit performed at commit `6ec5af51` (see #1324).

## ALTER DEFAULT PRIVILEGES / RBAC (8)

| File | Gap |
|------|-----|
| `98_alter_default_privileges_for_all_roles.sql` | ALTER DEFAULT PRIVILEGES FOR ALL ROLES |
| `99_alter_default_privileges_for_schema.sql` | ALTER DEFAULT PRIVILEGES FOR SCHEMA |
| `100_alter_default_privileges_for_sequence.sql` | ALTER DEFAULT PRIVILEGES FOR SEQUENCE |
| `101_alter_default_privileges_for_type.sql` | ALTER DEFAULT PRIVILEGES FOR TYPE |
| `102_alter_default_privileges_in_schema.sql` | ALTER DEFAULT PRIVILEGES IN SCHEMA |
| `103_alter_role.sql` | ALTER ROLE options |
| `175_pg_catalog_pg_default_acl_with_grant_option.sql` | pg_default_acl WITH GRANT OPTION |
| `202_target_names.sql` | Target name resolution for privileges |

## DDL gaps — ALTER TABLE / column type / schema changes (6)

| File | Gap |
|------|-----|
| `96_alter_column_type.sql` | ALTER COLUMN TYPE |
| `97_alter_database_convert_to_schema.sql` | ALTER DATABASE CONVERT TO SCHEMA |
| `115_comment_on.sql` | COMMENT ON (extended) |
| `121_database.sql` | Database DDL operations |
| `187_schema_locked.sql` | Schema locking |
| `205_truncate_with_concurrent_mutation.sql` | TRUNCATE with concurrent mutation |

## Collation support (4)

| File | Gap |
|------|-----|
| `111_collatedstring_normalization.sql` | Collated string normalization |
| `112_collatedstring_nullinindex.sql` | NULL in collated index |
| `113_collatedstring_uniqueindex1.sql` | Collated unique index (1) |
| `114_collatedstring_uniqueindex2.sql` | Collated unique index (2) |

## SQL expression / function gaps (12)

| File | Gap |
|------|-----|
| `94_aggregate.sql` | Aggregate edge cases |
| `95_alias_types.sql` | Alias type resolution |
| `104_and_or.sql` | AND/OR expression edge cases |
| `116_conditional.sql` | Conditional expression edge cases |
| `120_custom_escape_character.sql` | Custom ESCAPE character in LIKE |
| `140_function_lookup.sql` | Function lookup resolution |
| `146_impure.sql` | Impure function semantics |
| `169_name_escapes.sql` | Name escaping edge cases |
| `172_ordinality.sql` | WITH ORDINALITY |
| `173_overflow.sql` | Numeric overflow handling |
| `214_vectorize_agg.sql` | Vectorized aggregate execution |
| `215_vectorize_types.sql` | Vectorized type handling |

## Session / GUC behavior (4)

| File | Gap |
|------|-----|
| `98_session_settings.sql` | Session settings parity |
| `100_current_setting.sql` | current_setting() edge cases |
| `123_dist_vectorize.sql` | Distributed vectorize GUC |
| `150_int_size.sql` | Integer size GUC |

## Subquery / lateral / apply join (4)

| File | Gap |
|------|-----|
| `105_apply_join.sql` | Apply join (lateral) |
| `133_distsql_subquery.sql` | Distributed subquery execution |
| `162_lookup_join_local.sql` | Lookup join (local) |
| `200_suboperators.sql` | Sub-operators |

## Join execution (7)

| File | Gap |
|------|-----|
| `131_distsql_join.sql` | Distributed join execution |
| `143_group_join.sql` | Group join |
| `145_hash_join_dist.sql` | Distributed hash join |
| `149_inner-join.sql` | Inner join edge cases |
| `163_lookup_join_spans.sql` | Lookup join spans |
| `164_merge_join.sql` | Merge join |
| `165_merge_join_dist.sql` | Distributed merge join |

## DISTINCT / UNION / set operations (4)

| File | Gap |
|------|-----|
| `124_distinct.sql` | DISTINCT edge cases |
| `128_distsql_distinct_on.sql` | Distributed DISTINCT ON |
| `180_propagate_input_ordering.sql` | Input ordering propagation |
| `211_union.sql` | UNION edge cases |

## EXPLAIN output format (2)

| File | Gap |
|------|-----|
| `94_gin_index_query.sql` | GIN index query EXPLAIN |
| `138_explain.sql` | EXPLAIN format parity |

## UDF / procedure / routine (8)

| File | Gap |
|------|-----|
| `170_notice.sql` | RAISE NOTICE |
| `178_procedure_cte.sql` | Procedure with CTE |
| `179_procedure_deps.sql` | Procedure dependencies |
| `183_routine_schema_change.sql` | Routine schema change |
| `207_udf_cte.sql` | UDF with CTE |
| `208_udf_prepare.sql` | UDF with PREPARE |
| `209_udf_procedure_mix.sql` | UDF/procedure interop |
| `210_udf_rewrite.sql` | UDF rewrite |

## Catalog / introspection / SHOW (7)

| File | Gap |
|------|-----|
| `110_cluster_locks_write_buffering.sql` | Cluster lock introspection |
| `152_inv_stats.sql` | Inverted index statistics |
| `190_sequences_regclass.sql` | Sequence regclass resolution |
| `191_show_create_all_routines.sql` | SHOW CREATE ALL ROUTINES |
| `192_show_create_all_triggers.sql` | SHOW CREATE ALL TRIGGERS |
| `193_show_create_all_types.sql` | SHOW CREATE ALL TYPES |
| `194_show_create_redact.sql` | SHOW CREATE with redaction |

## INSERT / UPSERT / DML (4)

| File | Gap |
|------|-----|
| `119_create_as_non_metamorphic.sql` | CREATE AS (non-metamorphic) |
| `122_direct_columnar_scans.sql` | Direct columnar scans |
| `160_jsonb_path_exists_index_acceleration.sql` | JSONB path_exists index acceleration |
| `213_upsert_non_metamorphic.sql` | UPSERT (non-metamorphic) |

## Query execution / expression eval (5)

| File | Gap |
|------|-----|
| `127_distsql_datetime.sql` | Distributed datetime execution |
| `130_distsql_expr.sql` | Distributed expression evaluation |
| `132_distsql_numtables.sql` | Distributed multi-table execution |
| `141_generic.sql` | Generic query execution |
| `155_inverted_filter_json_array.sql` | Inverted filter JSON array |

## Transaction / savepoint (3)

| File | Gap |
|------|-----|
| `144_guardrails.sql` | Transaction guardrails |
| `174_partial_txn_commit.sql` | Partial transaction commit |
| `185_savepoints.sql` | Savepoint edge cases |

## Optimizer / planning (2)

| File | Gap |
|------|-----|
| `151_internal_executor.sql` | Internal executor |
| `171_optimizer_timeout.sql` | Optimizer timeout |

## Statistics (1)

| File | Gap |
|------|-----|
| `199_stats.sql` | Statistics edge cases |
