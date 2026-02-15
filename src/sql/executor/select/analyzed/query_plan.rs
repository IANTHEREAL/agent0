//! Single-point query routing gate.
//!
//! `QueryPlan` is produced ONCE from an `AnalyzedQuery`, capturing every
//! capability / routing decision in one place.  The executor reads from
//! the plan instead of re-computing `has_unresolved_subquery`,
//! `has_catalog_dependent_function`, etc. inline at 22+ sites.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect, SetOpKind, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use sqlparser::ast::{LockClause, LockType, NonBlock};
use std::fmt;

use super::materialize::has_catalog_dependent_function;
use super::rewrite::{has_aggregates, has_windows};
use super::subquery::has_unresolved_subquery;

// ── Public types ────────────────────────────────────────────────────

/// Produced ONCE after Analyzer, consumed by executor.
/// All routing decisions are made here — executor just follows the plan.
pub(crate) struct QueryPlan {
    pub path: ExecutionPath,
    pub features: QueryFeatures,
    pub where_strategy: WhereStrategy,
    pub order_by_strategy: OrderByStrategy,
    pub projection_strategy: ProjectionStrategy,
    pub distinct: DistinctStrategy,
    pub lock: LockInfo,
}

pub(crate) enum ExecutionPath {
    SetOperation { op: SetOpKind, all: bool },
    Tableless,
    SingleTable,
    Join,
}

pub(crate) struct QueryFeatures {
    pub has_aggregates: bool,
    pub has_windows: bool,
}

pub(crate) enum WhereStrategy {
    /// All sync — push to FilterOperator.
    AllSync,
    /// All async — per-row materialize (catalog-dependent functions).
    AllAsync,
    /// Split: sync conjuncts to FilterOperator, async remainder per-row.
    Split,
    /// No WHERE clause.
    None,
}

pub(crate) enum OrderByStrategy {
    /// Push to SortOperator (no subqueries / catalog funcs).
    Inline,
    /// Contains subqueries/catalog funcs — sort after projection.
    Deferred,
    /// No ORDER BY.
    None,
}

pub(crate) enum ProjectionStrategy {
    /// All sync — ProjectOperator handles it.
    Sync,
    /// Contains async expressions — per-row materialize_expr_for_row.
    NeedsMaterialization,
}

pub(crate) enum DistinctStrategy {
    /// No DISTINCT.
    None,
    /// DISTINCT — deduplicate after projection.
    Distinct,
    /// DISTINCT ON — separate path.
    DistinctOn,
}

pub(crate) struct LockInfo {
    pub has_for_update: bool,
    pub has_for_share: bool,
    pub has_lock: bool,
    pub has_skip_locked: bool,
    pub has_nowait: bool,
}

// ── Unsupported feature errors ──────────────────────────────────────

pub(crate) enum UnsupportedFeature {
    ForUpdateWithAggregate,
    ForUpdateWithDistinct,
    JoinWithForUpdate,
}

impl fmt::Display for UnsupportedFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForUpdateWithAggregate => write!(
                f,
                "FOR UPDATE/SHARE is not allowed with aggregate or window functions"
            ),
            Self::ForUpdateWithDistinct => {
                write!(f, "FOR UPDATE/SHARE is not allowed with DISTINCT")
            }
            Self::JoinWithForUpdate => {
                write!(f, "FOR UPDATE/SHARE is not allowed with JOIN queries")
            }
        }
    }
}

// ── plan_query — single entry point ─────────────────────────────────

/// Analyze an `AnalyzedQuery` and produce the execution plan.
///
/// ALL capability checks happen here.  The executor reads from the plan
/// without re-computing any predicates.
///
/// Returns `Err` only for unsupported feature combinations
/// (FOR UPDATE + aggregates, etc.).
pub(crate) fn plan_query(
    analyzed: &AnalyzedQuery,
    locks: &[LockClause],
) -> Result<QueryPlan, UnsupportedFeature> {
    // 1. Determine execution path.
    let path = determine_path(analyzed);

    // 2. Parse lock clauses.
    let lock = parse_locks(locks);

    // 3. SELECT-specific analysis (only meaningful for Select body).
    let (features, where_strategy, order_by_strategy, projection_strategy, distinct) =
        match &analyzed.body {
            AnalyzedQueryBody::Select(select) => {
                let features = analyze_features(select);
                let where_strategy = analyze_where(select);
                let order_by_strategy = analyze_order_by(&analyzed.order_by);
                let projection_strategy = analyze_projection(select);
                let distinct = analyze_distinct(select);

                // 4. Validate unsupported combinations.
                if lock.has_lock {
                    if features.has_aggregates || features.has_windows {
                        return Err(UnsupportedFeature::ForUpdateWithAggregate);
                    }
                    if !matches!(distinct, DistinctStrategy::None) {
                        return Err(UnsupportedFeature::ForUpdateWithDistinct);
                    }
                    if matches!(path, ExecutionPath::Join) {
                        return Err(UnsupportedFeature::JoinWithForUpdate);
                    }
                }

                (
                    features,
                    where_strategy,
                    order_by_strategy,
                    projection_strategy,
                    distinct,
                )
            }
            AnalyzedQueryBody::SetOperation { .. } => (
                QueryFeatures {
                    has_aggregates: false,
                    has_windows: false,
                },
                WhereStrategy::None,
                OrderByStrategy::None,
                ProjectionStrategy::Sync,
                DistinctStrategy::None,
            ),
        };

    Ok(QueryPlan {
        path,
        features,
        where_strategy,
        order_by_strategy,
        projection_strategy,
        distinct,
        lock,
    })
}

// ── Analysis helpers (private) ──────────────────────────────────────

fn determine_path(analyzed: &AnalyzedQuery) -> ExecutionPath {
    match &analyzed.body {
        AnalyzedQueryBody::SetOperation { op, all, .. } => {
            ExecutionPath::SetOperation { op: *op, all: *all }
        }
        AnalyzedQueryBody::Select(select) => {
            if select.from.is_empty() {
                ExecutionPath::Tableless
            } else {
                use crate::sql::analyzer::types::AnalyzedTableRefKind;
                let has_join = select.from.len() > 1
                    || select.from.first().map_or(false, |f| {
                        !matches!(f.kind, AnalyzedTableRefKind::Table { .. })
                    });
                if has_join {
                    ExecutionPath::Join
                } else {
                    ExecutionPath::SingleTable
                }
            }
        }
    }
}

fn parse_locks(locks: &[LockClause]) -> LockInfo {
    let has_for_update = locks
        .iter()
        .any(|l| matches!(l.lock_type, LockType::Update));
    let has_for_share = locks.iter().any(|l| matches!(l.lock_type, LockType::Share));
    let has_lock = has_for_update || has_for_share;
    let has_skip_locked = locks
        .iter()
        .any(|l| matches!(l.nonblock, Some(NonBlock::SkipLocked)));
    let has_nowait = locks
        .iter()
        .any(|l| matches!(l.nonblock, Some(NonBlock::Nowait)));
    LockInfo {
        has_for_update,
        has_for_share,
        has_lock,
        has_skip_locked,
        has_nowait,
    }
}

fn analyze_features(select: &AnalyzedSelect) -> QueryFeatures {
    QueryFeatures {
        has_aggregates: has_aggregates(select),
        has_windows: has_windows(select),
    }
}

fn analyze_where(select: &AnalyzedSelect) -> WhereStrategy {
    match &select.where_clause {
        None => WhereStrategy::None,
        Some(w) => {
            // Note: the actual WHERE expression may change after pre_materialize_async_exprs,
            // so this is a pre-analysis hint.  The executor re-checks the materialized WHERE
            // to decide sync/async split.  However, we capture the *initial* strategy here
            // so the pipeline trace is informative.
            if has_catalog_dependent_function(w) {
                WhereStrategy::AllAsync
            } else if has_unresolved_subquery(w) {
                WhereStrategy::Split
            } else {
                WhereStrategy::AllSync
            }
        }
    }
}

fn analyze_order_by(order_by: &[TypedOrderByExpr]) -> OrderByStrategy {
    if order_by.is_empty() {
        OrderByStrategy::None
    } else if order_by
        .iter()
        .any(|o| has_unresolved_subquery(&o.expr) || has_catalog_dependent_function(&o.expr))
    {
        OrderByStrategy::Deferred
    } else {
        OrderByStrategy::Inline
    }
}

fn analyze_projection(select: &AnalyzedSelect) -> ProjectionStrategy {
    // Check whether projection needs async materialization.
    let needs_mat = select
        .projection
        .iter()
        .any(|p| has_unresolved_subquery(&p.expr) || has_catalog_dependent_function(&p.expr));
    if needs_mat {
        ProjectionStrategy::NeedsMaterialization
    } else {
        ProjectionStrategy::Sync
    }
}

fn analyze_distinct(select: &AnalyzedSelect) -> DistinctStrategy {
    match &select.distinct {
        AnalyzedDistinct::All => DistinctStrategy::None,
        AnalyzedDistinct::Distinct => DistinctStrategy::Distinct,
        AnalyzedDistinct::DistinctOn(_) => DistinctStrategy::DistinctOn,
    }
}

// ── Pipeline trace ──────────────────────────────────────────────────

impl QueryPlan {
    /// One-line summary for `tracing::debug!(target: "pipeline", ...)`.
    pub fn trace_summary(&self) -> String {
        let path = match &self.path {
            ExecutionPath::SetOperation { op, all } => {
                let op_name = match op {
                    SetOpKind::Union => "union",
                    SetOpKind::Intersect => "intersect",
                    SetOpKind::Except => "except",
                };
                format!("path=set_op({}{})", op_name, if *all { "_all" } else { "" })
            }
            ExecutionPath::Tableless => "path=tableless".to_string(),
            ExecutionPath::SingleTable => "path=single_table".to_string(),
            ExecutionPath::Join => "path=join".to_string(),
        };

        let features = format!(
            "agg={} win={}",
            self.features.has_aggregates, self.features.has_windows
        );

        let where_str = match &self.where_strategy {
            WhereStrategy::AllSync => "where=sync",
            WhereStrategy::AllAsync => "where=async",
            WhereStrategy::Split => "where=split",
            WhereStrategy::None => "where=none",
        };

        let order_by_str = match &self.order_by_strategy {
            OrderByStrategy::Inline => "orderby=inline",
            OrderByStrategy::Deferred => "orderby=deferred",
            OrderByStrategy::None => "orderby=none",
        };

        let proj_str = match &self.projection_strategy {
            ProjectionStrategy::Sync => "projection=sync",
            ProjectionStrategy::NeedsMaterialization => "projection=needs_materialization",
        };

        let distinct_str = match &self.distinct {
            DistinctStrategy::None => "",
            DistinctStrategy::Distinct => " distinct=true",
            DistinctStrategy::DistinctOn => " distinct=on",
        };

        let lock_str = if self.lock.has_lock {
            if self.lock.has_skip_locked {
                " lock=skip_locked"
            } else if self.lock.has_nowait {
                " lock=nowait"
            } else if self.lock.has_for_update {
                " lock=for_update"
            } else {
                " lock=for_share"
            }
        } else {
            ""
        };

        format!(
            "{} {} {} {} {}{}{}",
            path, features, where_str, order_by_str, proj_str, distinct_str, lock_str
        )
    }
}

impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.trace_summary())
    }
}
