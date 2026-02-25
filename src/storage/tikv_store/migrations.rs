use super::*;
use crate::sql::binder::{extract_relation_references_from_query, RelationDep};
use sqlparser::ast::{Query, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

const VIEW_BINDINGS_MIGRATION_NAME: &str = "20260223_000001_view_relation_bindings_v2";
const VIEW_BINDINGS_MIGRATION_CHECKSUM: &str = "view_relation_bindings_v2";
const VIEW_BINDINGS_MIGRATION_SQL_PREVIEW: &str =
    "backfill persisted view/matview relation bindings from legacy deps";

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
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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
        let marker_exists = txn.get(migration_key.clone()).await?.is_some();

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

        let plan = plan_relation_binding_backfills(targets);
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

        let marker_action = view_bindings_migration_marker_action(&plan);
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

        txn.commit().await?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
