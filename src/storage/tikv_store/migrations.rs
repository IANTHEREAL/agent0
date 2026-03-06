use super::*;
use crate::model::ForeignKeyAction;
use crate::sql::binder::{extract_relation_references_from_query, RelationDep};
use crate::storage::backpressure::tikv_op;
use sqlparser::ast::{Query, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

const VIEW_BINDINGS_MIGRATION_NAME: &str = "20260223_000001_view_relation_bindings_v2";
const VIEW_BINDINGS_MIGRATION_CHECKSUM: &str = "view_relation_bindings_v2";
const VIEW_BINDINGS_MIGRATION_SQL_PREVIEW: &str =
    "backfill persisted view/matview relation bindings from legacy deps";
const NO_PK_FK_CASCADE_MIGRATION_NAME: &str = "20260306_000001_no_pk_fk_cascade_downgrade";
const NO_PK_FK_CASCADE_MIGRATION_CHECKSUM: &str = "no_pk_fk_cascade_downgrade_v1";
const NO_PK_FK_CASCADE_MIGRATION_SQL_PREVIEW: &str =
    "downgrade CASCADE/SET NULL/SET DEFAULT to NO ACTION on no-PK child tables";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelationBindingKind {
    View,
    MaterializedView,
}

impl RelationBindingKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::MaterializedView => "materialized view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationBindingBackfillTarget {
    db_id: u64,
    kind: RelationBindingKind,
    full_name: String,
    query_sql: String,
    deps: Vec<String>,
    has_persisted_bindings: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationBindingBackfillAction {
    db_id: u64,
    kind: RelationBindingKind,
    full_name: String,
    bindings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct NoPkFkDowngradeTarget {
    db_id: u64,
    table_name: String,
    fk_name: String,
    old_on_delete: ForeignKeyAction,
    old_on_update: ForeignKeyAction,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NoPkFkDowngradeApplyResult {
    fk_name_matched: bool,
    downgraded_constraints: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationBindingBackfillIssue {
    kind: RelationBindingKind,
    full_name: String,
    reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RelationBindingBackfillPlan {
    actions: Vec<RelationBindingBackfillAction>,
    skipped_existing: usize,
    unresolved: Vec<RelationBindingBackfillIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewBindingsMigrationMarkerAction {
    EnsurePresent,
    EnsureAbsent,
}

fn view_bindings_migration_marker_action(
    plan: &RelationBindingBackfillPlan,
) -> ViewBindingsMigrationMarkerAction {
    if plan.unresolved.is_empty() {
        ViewBindingsMigrationMarkerAction::EnsurePresent
    } else {
        ViewBindingsMigrationMarkerAction::EnsureAbsent
    }
}

fn parse_stored_query(sql: &str) -> Result<Query> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| anyhow!("failed to parse stored query: {}", e))?;
    match stmts.as_slice() {
        [Statement::Query(q)] => Ok(q.as_ref().clone()),
        _ => Err(anyhow!("stored SQL is not a single query")),
    }
}

fn parse_full_name(full: &str) -> Result<(String, String)> {
    let mut parts = full.splitn(3, '.');
    let schema = parts
        .next()
        .ok_or_else(|| anyhow!("invalid full name '{}'", full))?;
    let name = parts
        .next()
        .ok_or_else(|| anyhow!("invalid full name '{}'", full))?;
    if parts.next().is_some() || schema.is_empty() || name.is_empty() {
        return Err(anyhow!("invalid full name '{}'", full));
    }
    Ok((schema.to_string(), name.to_string()))
}

fn derive_relation_bindings_from_legacy_deps(
    relation_kind: &str,
    relation_full_name: &str,
    query_sql: &str,
    deps: &[String],
) -> Result<Vec<String>> {
    let query = parse_stored_query(query_sql)?;
    let raw_refs = extract_relation_references_from_query(&query);
    if raw_refs.is_empty() {
        if deps.is_empty() {
            return Ok(Vec::new());
        }
        return Err(anyhow!(
            "cannot backfill {} '{}' relation bindings: query has no relation references but deps are present",
            relation_kind,
            relation_full_name
        ));
    }

    let mut dep_set: HashSet<String> = HashSet::new();
    let mut deps_by_name: HashMap<String, Vec<String>> = HashMap::new();
    for dep in deps {
        let (_, name) = parse_full_name(dep).map_err(|_| {
            anyhow!(
                "cannot backfill {} '{}' relation bindings: invalid legacy dep '{}'",
                relation_kind,
                relation_full_name,
                dep
            )
        })?;
        if !dep_set.insert(dep.clone()) {
            continue;
        }
        deps_by_name.entry(name).or_default().push(dep.clone());
    }

    if dep_set.is_empty() {
        return Err(anyhow!(
            "cannot backfill {} '{}' relation bindings: legacy deps are empty",
            relation_kind,
            relation_full_name
        ));
    }

    let mut assigned: Vec<Option<String>> = vec![None; raw_refs.len()];
    let mut ambiguous_slots: Vec<(usize, Vec<String>)> = Vec::new();

    for (idx, raw_ref) in raw_refs.iter().enumerate() {
        match raw_ref {
            RelationDep::Qualified { schema, name } => {
                let full = format!("{}.{}", schema, name);
                if !dep_set.contains(&full) {
                    return Err(anyhow!(
                        "cannot backfill {} '{}' relation bindings: qualified reference '{}' not found in legacy deps",
                        relation_kind,
                        relation_full_name,
                        full
                    ));
                }
                assigned[idx] = Some(full);
            }
            RelationDep::Unqualified { name } => {
                let candidates = deps_by_name.get(name).cloned().unwrap_or_default();
                if candidates.is_empty() {
                    return Err(anyhow!(
                        "cannot backfill {} '{}' relation bindings: unqualified reference '{}' not found in legacy deps",
                        relation_kind,
                        relation_full_name,
                        name
                    ));
                }
                if candidates.len() == 1 {
                    assigned[idx] = candidates.into_iter().next();
                } else {
                    ambiguous_slots.push((idx, candidates));
                }
            }
        }
    }

    if !ambiguous_slots.is_empty() {
        fn search_unique_assignment(
            slot_idx: usize,
            ambiguous_slots: &[(usize, Vec<String>)],
            assigned: &mut [Option<String>],
            dep_set: &HashSet<String>,
            solution: &mut Option<Vec<String>>,
            multiple: &mut bool,
        ) {
            if *multiple {
                return;
            }

            if slot_idx == ambiguous_slots.len() {
                let Some(candidate_bindings) =
                    assigned.iter().cloned().collect::<Option<Vec<String>>>()
                else {
                    return;
                };
                let covered: HashSet<String> = candidate_bindings.iter().cloned().collect();
                if covered != *dep_set {
                    return;
                }
                if solution.is_some() {
                    *multiple = true;
                } else {
                    *solution = Some(candidate_bindings);
                }
                return;
            }

            let (bind_idx, candidates) = &ambiguous_slots[slot_idx];
            for candidate in candidates {
                assigned[*bind_idx] = Some(candidate.clone());
                search_unique_assignment(
                    slot_idx + 1,
                    ambiguous_slots,
                    assigned,
                    dep_set,
                    solution,
                    multiple,
                );
                if *multiple {
                    return;
                }
            }
            assigned[*bind_idx] = None;
        }

        let mut solution: Option<Vec<String>> = None;
        let mut multiple = false;
        search_unique_assignment(
            0,
            &ambiguous_slots,
            &mut assigned,
            &dep_set,
            &mut solution,
            &mut multiple,
        );

        if multiple {
            return Err(anyhow!(
                "cannot backfill {} '{}' relation bindings: ambiguous legacy deps for unqualified relation references; recreate the {} to persist exact bindings",
                relation_kind,
                relation_full_name,
                relation_kind
            ));
        }

        return solution.ok_or_else(|| {
            anyhow!(
                "cannot backfill {} '{}' relation bindings: legacy deps do not provide a consistent binding assignment",
                relation_kind,
                relation_full_name
            )
        });
    }

    let bindings = assigned
        .into_iter()
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| {
            anyhow!(
                "cannot backfill {} '{}' relation bindings: incomplete binding assignment",
                relation_kind,
                relation_full_name
            )
        })?;
    let covered: HashSet<String> = bindings.iter().cloned().collect();
    if covered != dep_set {
        return Err(anyhow!(
            "cannot backfill {} '{}' relation bindings: legacy deps cannot be fully covered by query references",
            relation_kind,
            relation_full_name
        ));
    }
    Ok(bindings)
}

fn plan_relation_binding_backfills(
    targets: Vec<RelationBindingBackfillTarget>,
) -> RelationBindingBackfillPlan {
    let mut plan = RelationBindingBackfillPlan::default();
    for target in targets {
        if target.has_persisted_bindings {
            plan.skipped_existing += 1;
            continue;
        }

        match derive_relation_bindings_from_legacy_deps(
            target.kind.as_str(),
            &target.full_name,
            &target.query_sql,
            &target.deps,
        ) {
            Ok(bindings) => {
                plan.actions.push(RelationBindingBackfillAction {
                    db_id: target.db_id,
                    kind: target.kind,
                    full_name: target.full_name,
                    bindings,
                });
            }
            Err(err) => {
                plan.unresolved.push(RelationBindingBackfillIssue {
                    kind: target.kind,
                    full_name: target.full_name,
                    reason: err.to_string(),
                });
            }
        }
    }
    plan
}

/// Computes the full migration outcome from a list of relation binding targets
/// and whether the marker currently exists. Returns the backfill plan and the
/// marker action that should be applied.
///
/// This is the pure decision core of `ensure_view_relation_bindings_migration`.
/// Extracted for testability without requiring a live TiKV connection.
fn compute_migration_outcome(
    targets: Vec<RelationBindingBackfillTarget>,
    _marker_exists: bool,
) -> (
    RelationBindingBackfillPlan,
    ViewBindingsMigrationMarkerAction,
) {
    let plan = plan_relation_binding_backfills(targets);
    let action = view_bindings_migration_marker_action(&plan);
    (plan, action)
}

fn find_no_pk_fk_cascade_targets(schemas: &[(u64, TableSchema)]) -> Vec<NoPkFkDowngradeTarget> {
    let mut targets = Vec::new();
    for (db_id, schema) in schemas {
        if !schema.pk_indices.is_empty() {
            continue;
        }
        for fk in &schema.foreign_keys {
            if fk.on_delete.requires_child_pk() || fk.on_update.requires_child_pk() {
                targets.push(NoPkFkDowngradeTarget {
                    db_id: *db_id,
                    table_name: schema.name.clone(),
                    fk_name: fk.name.clone(),
                    old_on_delete: fk.on_delete.clone(),
                    old_on_update: fk.on_update.clone(),
                });
            }
        }
    }
    targets
}

fn apply_no_pk_fk_cascade_downgrade(
    schema: &mut TableSchema,
    target: &NoPkFkDowngradeTarget,
) -> NoPkFkDowngradeApplyResult {
    let mut result = NoPkFkDowngradeApplyResult::default();
    for fk in &mut schema.foreign_keys {
        if fk.name != target.fk_name {
            continue;
        }

        result.fk_name_matched = true;
        let mut downgraded = false;
        if fk.on_delete.requires_child_pk() {
            fk.on_delete = ForeignKeyAction::NoAction;
            downgraded = true;
        }
        if fk.on_update.requires_child_pk() {
            fk.on_update = ForeignKeyAction::NoAction;
            downgraded = true;
        }
        if downgraded {
            result.downgraded_constraints += 1;
        }
    }
    result
}

impl TikvStore {
    pub async fn record_migration(
        &self,
        txn: &mut Transaction,
        record: MigrationRecord,
    ) -> Result<()> {
        let key = self.key(&encode_migration_key(&record.name));
        let data = bincode::serialize(&record).context("Failed to serialize migration record")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn list_migrations(&self, txn: &mut Transaction) -> Result<Vec<MigrationRecord>> {
        let prefix = encode_migration_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut migrations = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let record: MigrationRecord =
                bincode::deserialize(pair.value()).context("Failed to deserialize migration")?;
            migrations.push(record);
        }

        migrations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(migrations)
    }

    pub async fn ensure_view_relation_bindings_migration(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let migration_key = self.key(&encode_migration_key(VIEW_BINDINGS_MIGRATION_NAME));
        let marker_exists = tikv_op!(txn.get(migration_key.clone()).await)?.is_some();

        let mut targets: Vec<RelationBindingBackfillTarget> = Vec::new();
        let databases = self.list_databases(&mut txn).await?;
        for db in databases {
            let db_id = db.id;

            for view in self.list_views(&mut txn, db_id).await? {
                let full = view.full_name();
                let has_persisted_bindings = self
                    .get_view_relation_bindings(&mut txn, db_id, &full)
                    .await?
                    .is_some();
                targets.push(RelationBindingBackfillTarget {
                    db_id,
                    kind: RelationBindingKind::View,
                    full_name: full,
                    query_sql: view.query,
                    deps: view.deps,
                    has_persisted_bindings,
                });
            }

            for matview in self.list_materialized_views(&mut txn, db_id).await? {
                let full = matview.full_name();
                let has_persisted_bindings = self
                    .get_materialized_view_relation_bindings(&mut txn, db_id, &full)
                    .await?
                    .is_some();
                targets.push(RelationBindingBackfillTarget {
                    db_id,
                    kind: RelationBindingKind::MaterializedView,
                    full_name: full,
                    query_sql: matview.query,
                    deps: matview.deps,
                    has_persisted_bindings,
                });
            }
        }

        let (plan, marker_action) = compute_migration_outcome(targets, marker_exists);
        for action in &plan.actions {
            match action.kind {
                RelationBindingKind::View => {
                    self.set_view_relation_bindings(
                        &mut txn,
                        action.db_id,
                        &action.full_name,
                        action.bindings.clone(),
                    )
                    .await?;
                }
                RelationBindingKind::MaterializedView => {
                    self.set_materialized_view_relation_bindings(
                        &mut txn,
                        action.db_id,
                        &action.full_name,
                        action.bindings.clone(),
                    )
                    .await?;
                }
            }
        }

        let mut marker_updated = false;
        match marker_action {
            ViewBindingsMigrationMarkerAction::EnsurePresent => {
                if !marker_exists {
                    let applied_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .to_string();
                    let record = MigrationRecord {
                        name: VIEW_BINDINGS_MIGRATION_NAME.to_string(),
                        applied_at,
                        checksum: VIEW_BINDINGS_MIGRATION_CHECKSUM.to_string(),
                        sql_preview: VIEW_BINDINGS_MIGRATION_SQL_PREVIEW.to_string(),
                    };
                    self.record_migration(&mut txn, record).await?;
                    marker_updated = true;
                }
            }
            ViewBindingsMigrationMarkerAction::EnsureAbsent => {
                if marker_exists {
                    txn_delete(&mut txn, migration_key).await?;
                    marker_updated = true;
                }
            }
        }

        tikv_op!(txn.commit().await)?;

        if !plan.unresolved.is_empty() {
            tracing::warn!(
                "Migration {} remains incomplete: unresolved={}, backfilled={}, already_persisted={}, marker_cleared={}, will_retry_on_next_startup=true",
                VIEW_BINDINGS_MIGRATION_NAME,
                plan.unresolved.len(),
                plan.actions.len(),
                plan.skipped_existing,
                marker_updated
            );
            for issue in plan.unresolved.iter().take(20) {
                tracing::warn!(
                    "Skipped binding backfill for {} '{}': {}",
                    issue.kind.as_str(),
                    issue.full_name,
                    issue.reason
                );
            }
        } else {
            info!(
                "Migration {} complete (backfilled={}, already_persisted={}, marker_written={})",
                VIEW_BINDINGS_MIGRATION_NAME,
                plan.actions.len(),
                plan.skipped_existing,
                marker_updated
            );
        }
        Ok(())
    }

    pub async fn ensure_no_pk_fk_cascade_migration(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let migration_key = self.key(&encode_migration_key(NO_PK_FK_CASCADE_MIGRATION_NAME));
        if tikv_op!(txn.get(migration_key).await)?.is_some() {
            tikv_op!(txn.commit().await)?;
            info!(
                "Migration {} already applied; skipping",
                NO_PK_FK_CASCADE_MIGRATION_NAME
            );
            return Ok(());
        }

        let databases = self.list_databases(&mut txn).await?;
        let mut schemas: Vec<(u64, TableSchema)> = Vec::new();
        for db in databases {
            let table_names = self.list_tables(&mut txn, db.id).await?;
            let table_schemas = self
                .list_table_schemas(&mut txn, db.id, &table_names)
                .await?;
            schemas.extend(table_schemas.into_iter().map(|schema| (db.id, schema)));
        }

        let targets = find_no_pk_fk_cascade_targets(&schemas);
        let mut tables_affected: HashSet<(u64, String)> = HashSet::new();
        let mut fks_downgraded = 0usize;
        for target in targets {
            let mut schema = self
                .get_schema(&mut txn, target.db_id, &target.table_name)
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "table '{}' not found during migration {}",
                        target.table_name,
                        NO_PK_FK_CASCADE_MIGRATION_NAME
                    )
                })?;

            let apply_result = apply_no_pk_fk_cascade_downgrade(&mut schema, &target);
            if !apply_result.fk_name_matched {
                return Err(anyhow!(
                    "constraint '{}' not found on table '{}' during migration {}",
                    target.fk_name,
                    target.table_name,
                    NO_PK_FK_CASCADE_MIGRATION_NAME
                ));
            }

            if apply_result.downgraded_constraints > 0 {
                self.update_schema(&mut txn, target.db_id, schema).await?;
                tables_affected.insert((target.db_id, target.table_name));
                fks_downgraded += apply_result.downgraded_constraints;
            }
        }

        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let record = MigrationRecord {
            name: NO_PK_FK_CASCADE_MIGRATION_NAME.to_string(),
            applied_at,
            checksum: NO_PK_FK_CASCADE_MIGRATION_CHECKSUM.to_string(),
            sql_preview: NO_PK_FK_CASCADE_MIGRATION_SQL_PREVIEW.to_string(),
        };
        self.record_migration(&mut txn, record).await?;
        tikv_op!(txn.commit().await)?;

        info!(
            "Migration {} complete (tables_affected={}, fks_downgraded={})",
            NO_PK_FK_CASCADE_MIGRATION_NAME,
            tables_affected.len(),
            fks_downgraded
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForeignKeyConstraint;

    fn target(
        db_id: u64,
        kind: RelationBindingKind,
        full_name: &str,
        query_sql: &str,
        deps: &[&str],
        has_persisted_bindings: bool,
    ) -> RelationBindingBackfillTarget {
        RelationBindingBackfillTarget {
            db_id,
            kind,
            full_name: full_name.to_string(),
            query_sql: query_sql.to_string(),
            deps: deps.iter().map(ToString::to_string).collect(),
            has_persisted_bindings,
        }
    }

    #[test]
    fn backfill_plan_success_and_existing_skip() {
        let plan = plan_relation_binding_backfills(vec![
            target(
                42,
                RelationBindingKind::View,
                "public.v1",
                "SELECT 1 FROM a.t qa JOIN t qz ON qa.id = qz.id",
                &["a.t", "z.t"],
                false,
            ),
            target(
                42,
                RelationBindingKind::MaterializedView,
                "public.mv1",
                "SELECT 1 FROM public.t",
                &["public.t"],
                true,
            ),
        ]);

        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.skipped_existing, 1);
        assert!(plan.unresolved.is_empty());

        let action = &plan.actions[0];
        assert_eq!(action.db_id, 42);
        assert_eq!(action.kind, RelationBindingKind::View);
        assert_eq!(action.full_name, "public.v1");
        assert_eq!(action.bindings, vec!["a.t".to_string(), "z.t".to_string()]);
    }

    #[test]
    fn backfill_plan_ambiguous_legacy_deps_is_non_fatal() {
        let plan = plan_relation_binding_backfills(vec![target(
            7,
            RelationBindingKind::View,
            "public.v_bad",
            "SELECT 1 FROM t a JOIN t b ON a.id = b.id",
            &["a.t", "z.t"],
            false,
        )]);

        assert!(plan.actions.is_empty());
        assert_eq!(plan.skipped_existing, 0);
        assert_eq!(plan.unresolved.len(), 1);
        let issue = &plan.unresolved[0];
        assert_eq!(issue.kind, RelationBindingKind::View);
        assert_eq!(issue.full_name, "public.v_bad");
        assert!(issue.reason.contains("ambiguous legacy deps"));
    }

    #[test]
    fn backfill_plan_restart_is_idempotent_after_backfill() {
        let first = plan_relation_binding_backfills(vec![target(
            99,
            RelationBindingKind::MaterializedView,
            "public.mv_restart",
            "SELECT 1 FROM public.t",
            &["public.t"],
            false,
        )]);
        assert_eq!(first.actions.len(), 1);
        assert!(first.unresolved.is_empty());

        let second = plan_relation_binding_backfills(vec![target(
            99,
            RelationBindingKind::MaterializedView,
            "public.mv_restart",
            "SELECT 1 FROM public.t",
            &["public.t"],
            true,
        )]);
        assert!(second.actions.is_empty());
        assert_eq!(second.skipped_existing, 1);
        assert!(second.unresolved.is_empty());
    }

    #[test]
    fn marker_action_requires_completion_for_applied_state() {
        let complete_plan = RelationBindingBackfillPlan {
            actions: vec![RelationBindingBackfillAction {
                db_id: 1,
                kind: RelationBindingKind::View,
                full_name: "public.v1".to_string(),
                bindings: vec!["public.t".to_string()],
            }],
            skipped_existing: 0,
            unresolved: vec![],
        };
        assert_eq!(
            view_bindings_migration_marker_action(&complete_plan),
            ViewBindingsMigrationMarkerAction::EnsurePresent
        );

        let incomplete_plan = RelationBindingBackfillPlan {
            actions: vec![],
            skipped_existing: 0,
            unresolved: vec![RelationBindingBackfillIssue {
                kind: RelationBindingKind::View,
                full_name: "public.v_bad".to_string(),
                reason: "ambiguous legacy deps".to_string(),
            }],
        };
        assert_eq!(
            view_bindings_migration_marker_action(&incomplete_plan),
            ViewBindingsMigrationMarkerAction::EnsureAbsent
        );
    }

    #[test]
    fn parse_full_name_rejects_invalid_shapes() {
        assert!(parse_full_name("public.t").is_ok());
        assert!(parse_full_name("public").is_err());
        assert!(parse_full_name(".t").is_err());
        assert!(parse_full_name("public.").is_err());
        assert!(parse_full_name("a.b.c").is_err());
    }

    #[test]
    fn parse_stored_query_requires_single_query_statement() {
        assert!(parse_stored_query("SELECT 1").is_ok());
        assert!(parse_stored_query("CREATE TABLE t(id INT)").is_err());
        assert!(parse_stored_query("SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn derive_bindings_allows_constant_query_with_empty_deps() {
        let bindings =
            derive_relation_bindings_from_legacy_deps("view", "public.v", "SELECT 1", &[]).unwrap();
        assert!(bindings.is_empty());
    }

    #[test]
    fn derive_bindings_errors_on_invalid_legacy_dep_shape() {
        let err = derive_relation_bindings_from_legacy_deps(
            "view",
            "public.v",
            "SELECT 1 FROM t",
            &["not_a_full_name".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid legacy dep"));
    }

    #[test]
    fn derive_bindings_errors_when_legacy_deps_empty_for_relation_query() {
        let err = derive_relation_bindings_from_legacy_deps(
            "view",
            "public.v",
            "SELECT 1 FROM public.t",
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("legacy deps are empty"));
    }

    /// Apply the marker action to the simulated in-memory marker state (true = marker present).
    fn apply_marker_action(
        _marker_present: bool,
        action: ViewBindingsMigrationMarkerAction,
    ) -> bool {
        match action {
            ViewBindingsMigrationMarkerAction::EnsurePresent => true,
            ViewBindingsMigrationMarkerAction::EnsureAbsent => false,
        }
    }

    /// Integration test: full marker lifecycle across three simulated startups.
    ///
    /// Phase 1 — first run with an unresolvable view: the backfill plan records an
    ///           unresolved entry, so the marker must stay absent.
    /// Phase 2 — next startup retry: the same unresolvable state is encountered again;
    ///           the marker must remain absent (migration retries on every startup).
    /// Phase 3 — after the legacy objects are fixed (deps become unambiguous): the plan
    ///           produces a backfill action and no unresolved entries, so the marker is
    ///           written and the migration converges.
    #[test]
    fn marker_lifecycle_unresolved_retried_then_resolved() {
        // Phase 1: first startup — ambiguous deps prevent backfill.
        // The query references the unqualified table 't' twice, but the two legacy
        // deps ("s1.t", "s2.t") cannot be uniquely assigned → unresolvable.
        let unresolvable = vec![target(
            1,
            RelationBindingKind::View,
            "public.v_ambig",
            "SELECT 1 FROM t a JOIN t b ON a.id = b.id",
            &["s1.t", "s2.t"],
            false,
        )];

        let plan1 = plan_relation_binding_backfills(unresolvable.clone());
        assert_eq!(
            plan1.unresolved.len(),
            1,
            "Phase 1: should have one unresolved entry"
        );
        assert!(
            plan1.actions.is_empty(),
            "Phase 1: no backfill actions expected"
        );

        let action1 = view_bindings_migration_marker_action(&plan1);
        assert_eq!(
            action1,
            ViewBindingsMigrationMarkerAction::EnsureAbsent,
            "Phase 1: marker must be absent when backfill is unresolved"
        );

        let mut marker_present = false; // initial state: no marker
        marker_present = apply_marker_action(marker_present, action1);
        assert!(!marker_present, "Phase 1: marker must remain absent");

        // Phase 2: next startup — same unresolvable state, migration retries.
        let plan2 = plan_relation_binding_backfills(unresolvable);
        assert!(
            !plan2.unresolved.is_empty(),
            "Phase 2: still unresolved on retry"
        );

        let action2 = view_bindings_migration_marker_action(&plan2);
        assert_eq!(
            action2,
            ViewBindingsMigrationMarkerAction::EnsureAbsent,
            "Phase 2: marker must stay absent on retry"
        );

        marker_present = apply_marker_action(marker_present, action2);
        assert!(!marker_present, "Phase 2: marker must still be absent");

        // Phase 3: after fix — the view is recreated with unambiguous, qualified deps.
        // The qualified reference resolves directly from the single dep entry.
        let resolvable = vec![target(
            1,
            RelationBindingKind::View,
            "public.v_ambig",
            "SELECT 1 FROM public.t",
            &["public.t"],
            false,
        )];

        let plan3 = plan_relation_binding_backfills(resolvable);
        assert!(
            plan3.unresolved.is_empty(),
            "Phase 3: all targets should resolve"
        );
        assert_eq!(
            plan3.actions.len(),
            1,
            "Phase 3: one backfill action expected"
        );
        assert_eq!(plan3.actions[0].bindings, vec!["public.t".to_string()]);

        let action3 = view_bindings_migration_marker_action(&plan3);
        assert_eq!(
            action3,
            ViewBindingsMigrationMarkerAction::EnsurePresent,
            "Phase 3: marker must be written after convergence"
        );

        marker_present = apply_marker_action(marker_present, action3);
        assert!(
            marker_present,
            "Phase 3: marker must be present after migration converges"
        );
    }

    /// Integration test: a pre-existing marker is cleared when a new unresolvable view
    /// appears, then re-written once all views become resolvable again.
    ///
    /// This validates the "clear/retry" semantic: an incomplete run must never leave a
    /// stale marker that would suppress future retries.
    #[test]
    fn stale_marker_cleared_when_unresolvable_view_appears() {
        // Pre-condition: migration previously completed; marker is present.
        let mut marker_present = true;

        // A new unresolvable view is added alongside the already-backfilled one.
        let targets_with_new_bad_view = vec![
            // Previously backfilled — has persisted bindings, skip.
            target(
                1,
                RelationBindingKind::View,
                "public.v_ok",
                "SELECT 1 FROM public.t",
                &["public.t"],
                true,
            ),
            // Newly added view with ambiguous legacy deps — cannot be backfilled.
            target(
                1,
                RelationBindingKind::View,
                "public.v_new_bad",
                "SELECT 1 FROM t a JOIN t b ON a.id = b.id",
                &["s1.t", "s2.t"],
                false,
            ),
        ];

        let plan1 = plan_relation_binding_backfills(targets_with_new_bad_view);
        assert_eq!(plan1.skipped_existing, 1, "existing view should be skipped");
        assert_eq!(plan1.unresolved.len(), 1, "new bad view must be unresolved");
        assert!(plan1.actions.is_empty());

        let action1 = view_bindings_migration_marker_action(&plan1);
        assert_eq!(
            action1,
            ViewBindingsMigrationMarkerAction::EnsureAbsent,
            "stale marker must be cleared when unresolved entries exist"
        );

        marker_present = apply_marker_action(marker_present, action1);
        assert!(!marker_present, "stale marker must be absent after clear");

        // After the bad view is fixed, both views can be resolved.
        let targets_fixed = vec![
            target(
                1,
                RelationBindingKind::View,
                "public.v_ok",
                "SELECT 1 FROM public.t",
                &["public.t"],
                true, // still has persisted bindings
            ),
            target(
                1,
                RelationBindingKind::View,
                "public.v_new_bad",
                "SELECT 1 FROM public.t",
                &["public.t"],
                false, // recreated, now unambiguous
            ),
        ];

        let plan2 = plan_relation_binding_backfills(targets_fixed);
        assert!(
            plan2.unresolved.is_empty(),
            "all targets should resolve after fix"
        );
        assert_eq!(plan2.skipped_existing, 1);
        assert_eq!(plan2.actions.len(), 1);

        let action2 = view_bindings_migration_marker_action(&plan2);
        assert_eq!(
            action2,
            ViewBindingsMigrationMarkerAction::EnsurePresent,
            "marker must be re-written after all views resolve"
        );

        marker_present = apply_marker_action(marker_present, action2);
        assert!(
            marker_present,
            "marker must be present after migration re-converges"
        );
    }

    #[test]
    fn migration_outcome_no_views_writes_marker() {
        // Empty cluster: no views to backfill, plan is complete, marker should be written.
        let (plan, action) = compute_migration_outcome(vec![], false);
        assert!(plan.actions.is_empty());
        assert!(plan.unresolved.is_empty());
        assert_eq!(action, ViewBindingsMigrationMarkerAction::EnsurePresent);
    }

    #[test]
    fn migration_outcome_unresolvable_keeps_marker_absent() {
        // A view with ambiguous legacy deps: backfill can't complete, marker must stay absent.
        let targets = vec![target(
            1,
            RelationBindingKind::View,
            "public.v_ambig",
            "SELECT 1 FROM t a JOIN t b ON a.id = b.id",
            &["s1.t", "s2.t"],
            false,
        )];
        let (plan, action) = compute_migration_outcome(targets, false);
        assert_eq!(plan.unresolved.len(), 1);
        assert!(plan.actions.is_empty());
        assert_eq!(action, ViewBindingsMigrationMarkerAction::EnsureAbsent);
    }

    #[test]
    fn migration_outcome_already_backfilled_writes_marker() {
        // View already has persisted bindings: skipped, plan complete, marker written.
        let targets = vec![target(
            1,
            RelationBindingKind::View,
            "public.v1",
            "SELECT 1 FROM public.t",
            &["public.t"],
            true, // has_persisted_bindings
        )];
        let (plan, action) = compute_migration_outcome(targets, false);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.skipped_existing, 1);
        assert!(plan.unresolved.is_empty());
        assert_eq!(action, ViewBindingsMigrationMarkerAction::EnsurePresent);
    }

    #[test]
    fn migration_outcome_full_lifecycle_converges() {
        // Phase 1: unresolvable view → marker absent
        let unresolvable = vec![target(
            1,
            RelationBindingKind::View,
            "public.v",
            "SELECT 1 FROM t a JOIN t b ON a.id = b.id",
            &["s1.t", "s2.t"],
            false,
        )];
        let (_, action1) = compute_migration_outcome(unresolvable, false);
        assert_eq!(action1, ViewBindingsMigrationMarkerAction::EnsureAbsent);

        // Phase 2: view fixed (unambiguous deps) → plan has action → marker written
        let resolvable = vec![target(
            1,
            RelationBindingKind::View,
            "public.v",
            "SELECT 1 FROM public.t",
            &["public.t"],
            false,
        )];
        let (plan2, action2) = compute_migration_outcome(resolvable, false);
        assert_eq!(plan2.actions.len(), 1);
        assert!(plan2.unresolved.is_empty());
        assert_eq!(action2, ViewBindingsMigrationMarkerAction::EnsurePresent);

        // Phase 3: idempotent restart — bindings already persisted → skip → marker still written
        let after_backfill = vec![target(
            1,
            RelationBindingKind::View,
            "public.v",
            "SELECT 1 FROM public.t",
            &["public.t"],
            true,
        )];
        let (plan3, action3) = compute_migration_outcome(after_backfill, true);
        assert!(plan3.actions.is_empty());
        assert_eq!(plan3.skipped_existing, 1);
        assert_eq!(action3, ViewBindingsMigrationMarkerAction::EnsurePresent);
    }

    fn test_fk(
        name: &str,
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    ) -> ForeignKeyConstraint {
        ForeignKeyConstraint {
            name: name.to_string(),
            columns: vec!["child_id".to_string()],
            ref_table: "public.parent".to_string(),
            ref_columns: vec!["id".to_string()],
            on_delete,
            on_update,
        }
    }

    fn test_schema(
        table_name: &str,
        pk_indices: Vec<usize>,
        foreign_keys: Vec<ForeignKeyConstraint>,
    ) -> TableSchema {
        TableSchema {
            name: table_name.to_string(),
            pk_indices,
            foreign_keys,
            ..TableSchema::default()
        }
    }

    fn no_pk_target(table_name: &str, fk_name: &str) -> NoPkFkDowngradeTarget {
        NoPkFkDowngradeTarget {
            db_id: 1,
            table_name: table_name.to_string(),
            fk_name: fk_name.to_string(),
            old_on_delete: ForeignKeyAction::Cascade,
            old_on_update: ForeignKeyAction::NoAction,
        }
    }

    #[test]
    fn find_no_pk_fk_cascade_targets_empty_for_pk_tables() {
        let schemas = vec![(
            1,
            test_schema(
                "public.child",
                vec![0],
                vec![test_fk(
                    "child_parent_fk",
                    ForeignKeyAction::Cascade,
                    ForeignKeyAction::NoAction,
                )],
            ),
        )];
        let targets = find_no_pk_fk_cascade_targets(&schemas);
        assert!(targets.is_empty());
    }

    #[test]
    fn find_no_pk_fk_cascade_targets_empty_for_safe_actions() {
        let schemas = vec![(
            1,
            test_schema(
                "public.child",
                vec![],
                vec![
                    test_fk(
                        "child_parent_fk_1",
                        ForeignKeyAction::NoAction,
                        ForeignKeyAction::Restrict,
                    ),
                    test_fk(
                        "child_parent_fk_2",
                        ForeignKeyAction::Restrict,
                        ForeignKeyAction::NoAction,
                    ),
                ],
            ),
        )];
        let targets = find_no_pk_fk_cascade_targets(&schemas);
        assert!(targets.is_empty());
    }

    #[test]
    fn find_no_pk_fk_cascade_targets_detects_on_delete_cascade() {
        let schemas = vec![(
            9,
            test_schema(
                "public.child",
                vec![],
                vec![test_fk(
                    "child_parent_fk",
                    ForeignKeyAction::Cascade,
                    ForeignKeyAction::NoAction,
                )],
            ),
        )];
        let targets = find_no_pk_fk_cascade_targets(&schemas);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].db_id, 9);
        assert_eq!(targets[0].table_name, "public.child");
        assert_eq!(targets[0].fk_name, "child_parent_fk");
        assert_eq!(targets[0].old_on_delete, ForeignKeyAction::Cascade);
        assert_eq!(targets[0].old_on_update, ForeignKeyAction::NoAction);
    }

    #[test]
    fn find_no_pk_fk_cascade_targets_detects_on_update_set_null() {
        let schemas = vec![(
            5,
            test_schema(
                "public.child",
                vec![],
                vec![test_fk(
                    "child_parent_fk",
                    ForeignKeyAction::NoAction,
                    ForeignKeyAction::SetNull,
                )],
            ),
        )];
        let targets = find_no_pk_fk_cascade_targets(&schemas);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].db_id, 5);
        assert_eq!(targets[0].table_name, "public.child");
        assert_eq!(targets[0].fk_name, "child_parent_fk");
        assert_eq!(targets[0].old_on_delete, ForeignKeyAction::NoAction);
        assert_eq!(targets[0].old_on_update, ForeignKeyAction::SetNull);
    }

    #[test]
    fn find_no_pk_fk_cascade_targets_detects_mixed_unsafe_actions() {
        let schemas = vec![(
            2,
            test_schema(
                "public.child",
                vec![],
                vec![test_fk(
                    "child_parent_fk",
                    ForeignKeyAction::Cascade,
                    ForeignKeyAction::SetNull,
                )],
            ),
        )];
        let targets = find_no_pk_fk_cascade_targets(&schemas);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].db_id, 2);
        assert_eq!(targets[0].table_name, "public.child");
        assert_eq!(targets[0].fk_name, "child_parent_fk");
        assert_eq!(targets[0].old_on_delete, ForeignKeyAction::Cascade);
        assert_eq!(targets[0].old_on_update, ForeignKeyAction::SetNull);
    }

    #[test]
    fn apply_no_pk_fk_cascade_downgrade_handles_duplicate_fk_names() {
        let mut schema = test_schema(
            "public.child",
            vec![],
            vec![
                test_fk(
                    "dup_fk",
                    ForeignKeyAction::NoAction,
                    ForeignKeyAction::NoAction,
                ),
                test_fk(
                    "dup_fk",
                    ForeignKeyAction::Cascade,
                    ForeignKeyAction::NoAction,
                ),
            ],
        );
        let target = no_pk_target("public.child", "dup_fk");
        let result = apply_no_pk_fk_cascade_downgrade(&mut schema, &target);

        assert!(result.fk_name_matched);
        assert_eq!(result.downgraded_constraints, 1);
        assert_eq!(schema.foreign_keys[0].on_delete, ForeignKeyAction::NoAction);
        assert_eq!(schema.foreign_keys[1].on_delete, ForeignKeyAction::NoAction);
    }

    #[test]
    fn apply_no_pk_fk_cascade_downgrade_downgrades_all_unsafe_same_name_entries() {
        let mut schema = test_schema(
            "public.child",
            vec![],
            vec![
                test_fk(
                    "dup_fk",
                    ForeignKeyAction::Cascade,
                    ForeignKeyAction::NoAction,
                ),
                test_fk(
                    "dup_fk",
                    ForeignKeyAction::NoAction,
                    ForeignKeyAction::SetNull,
                ),
            ],
        );
        let target = no_pk_target("public.child", "dup_fk");
        let result = apply_no_pk_fk_cascade_downgrade(&mut schema, &target);

        assert!(result.fk_name_matched);
        assert_eq!(result.downgraded_constraints, 2);
        assert_eq!(schema.foreign_keys[0].on_delete, ForeignKeyAction::NoAction);
        assert_eq!(schema.foreign_keys[1].on_update, ForeignKeyAction::NoAction);
    }

    #[test]
    fn apply_no_pk_fk_cascade_downgrade_is_idempotent_on_rerun() {
        let mut schema = test_schema(
            "public.child",
            vec![],
            vec![test_fk(
                "dup_fk",
                ForeignKeyAction::Cascade,
                ForeignKeyAction::SetDefault,
            )],
        );
        let target = no_pk_target("public.child", "dup_fk");

        let first = apply_no_pk_fk_cascade_downgrade(&mut schema, &target);
        let second = apply_no_pk_fk_cascade_downgrade(&mut schema, &target);

        assert!(first.fk_name_matched);
        assert_eq!(first.downgraded_constraints, 1);
        assert!(second.fk_name_matched);
        assert_eq!(second.downgraded_constraints, 0);
        assert_eq!(schema.foreign_keys[0].on_delete, ForeignKeyAction::NoAction);
        assert_eq!(schema.foreign_keys[0].on_update, ForeignKeyAction::NoAction);
    }
}
