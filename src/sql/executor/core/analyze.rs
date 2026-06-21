//! ANALYZE command — collects per-column statistics for the query planner.
//!
//! ## Transaction contract
//!
//! - `ANALYZE <table>` uses the caller transaction (or a single implicit
//!   autocommit transaction).
//! - Bare `ANALYZE` inside an explicit transaction block remains a single
//!   transaction operation.
//! - Bare `ANALYZE` in autocommit mode discovers the table set first, then
//!   analyzes each table in its own autocommit transaction (with retry on
//!   retryable TiKV conflicts). This is intentionally non-atomic across tables.

use super::*;
use crate::auth::Privilege;
use crate::model::TableSchema;
use crate::sql::error::SqlError;
use crate::sql::expr::operators::compare_values;
use crate::sql::operators::key_encoding::{canonicalize_value, encode_value_key};
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};

/// Maximum number of most-common-values to retain per column.
const MCV_LIMIT: usize = 10;

/// Number of equi-depth histogram buckets.
const HISTOGRAM_BUCKETS: usize = 100;

/// Batch size (rows) for streaming table scan.
const ANALYZE_BATCH_SIZE: u32 = 10_000;

/// Maximum width (bytes) of a value to include in MCV or histogram statistics.
/// Values wider than this are skipped — they are unlikely to be duplicated
/// and would inflate the per-column statistics blob.
/// Matches PostgreSQL's `WIDTH_THRESHOLD` in `analyze.c`.
const WIDTH_THRESHOLD: usize = 1024;

// ── Table name parsing ──────────────────────────────────────────────────

/// Parse the optional table name from a raw ANALYZE SQL string.
///
/// Returns `Ok(None)` for bare `ANALYZE`, `Ok(Some(name))` for a specific
/// table, or `Err` for syntax errors.  Handles quoted identifiers and
/// optional VERBOSE keyword by delegating to sqlparser.
pub(crate) fn parse_analyze_table_name(sql: &str) -> Result<Option<sqlparser::ast::ObjectName>> {
    // Skip the "ANALYZE" keyword (7 bytes).
    let tail = sql.get(7..).unwrap_or("");
    let rest = crate::sql::raw_sql::skip_ws_and_comments(tail)
        .ok_or_else(|| anyhow!("syntax error in ANALYZE: unterminated /* comment"))?;
    if rest.is_empty() || rest == ";" {
        return Ok(None); // bare ANALYZE
    }

    // Skip optional VERBOSE keyword.
    let after_verbose = if rest.len() >= 7
        && rest.as_bytes()[..7].eq_ignore_ascii_case(b"VERBOSE")
        && (rest.len() == 7 || rest.as_bytes()[7].is_ascii_whitespace())
    {
        let v_rest = crate::sql::raw_sql::skip_ws_and_comments(rest.get(7..).unwrap_or(""))
            .ok_or_else(|| anyhow!("syntax error in ANALYZE: unterminated /* comment"))?;
        if v_rest.is_empty() || v_rest == ";" {
            return Ok(None); // ANALYZE VERBOSE (bare, with verbose)
        }
        v_rest
    } else {
        rest
    };

    // Strip trailing semicolons and whitespace for the table name portion.
    let table_part = after_verbose.trim_end_matches(';').trim();
    if table_part.is_empty() {
        return Ok(None);
    }

    // Use sqlparser to parse: wrap in "SELECT 1 FROM <name>" for robust
    // identifier parsing (handles quoting, schema qualification, etc.).
    let synthetic = format!("SELECT 1 FROM {}", table_part);
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let stmts = sqlparser::parser::Parser::parse_sql(&dialect, &synthetic)
        .map_err(|e| anyhow!("invalid table name in ANALYZE: {}", e))?;

    // Strict extraction: exactly one statement.
    if stmts.len() != 1 {
        return Err(anyhow!(
            "syntax error in ANALYZE: expected a single table name"
        ));
    }
    let query = match &stmts[0] {
        sqlparser::ast::Statement::Query(q) => q.as_ref(),
        _ => return Err(anyhow!("syntax error in ANALYZE: expected a table name")),
    };

    // Reject Query-level trailing clauses (ORDER BY, LIMIT, OFFSET, etc.).
    if !query.order_by.is_empty()
        || query.limit.is_some()
        || query.offset.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.with.is_some()
        || !query.limit_by.is_empty()
        || query.for_clause.is_some()
    {
        return Err(anyhow!(
            "syntax error in ANALYZE: unexpected clause after table name"
        ));
    }

    let body = query.body.as_ref();
    let select = match body {
        sqlparser::ast::SetExpr::Select(sel) => sel.as_ref(),
        _ => return Err(anyhow!("syntax error in ANALYZE: expected a table name")),
    };

    // Reject trailing junk (WHERE, GROUP BY, etc.).
    if select.selection.is_some()
        || matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs) if !exprs.is_empty())
        || select.having.is_some()
    {
        return Err(anyhow!(
            "syntax error in ANALYZE: unexpected clause after table name"
        ));
    }

    // Exactly one table factor, no joins.
    if select.from.len() != 1 {
        return Err(anyhow!(
            "syntax error in ANALYZE: expected exactly one table"
        ));
    }
    let from = &select.from[0];
    if !from.joins.is_empty() {
        return Err(anyhow!(
            "syntax error in ANALYZE: unexpected JOIN in ANALYZE"
        ));
    }

    match &from.relation {
        sqlparser::ast::TableFactor::Table { name, alias, .. } => {
            // Reject aliases (e.g. "ANALYZE users u").
            if alias.is_some() {
                return Err(anyhow!(
                    "syntax error in ANALYZE: unexpected alias after table name"
                ));
            }
            Ok(Some(name.clone()))
        }
        _ => Err(anyhow!("syntax error in ANALYZE: expected a table name")),
    }
}

// ── Datum width estimation (PG-compatible) ─────────────────────────────

use crate::sql::types::sizing;

/// Estimate datum width (bytes) matching PostgreSQL's on-disk column size
/// semantics.  Used for WIDTH_THRESHOLD gating during ANALYZE.
///
/// Delegates to shared helpers in `crate::sql::types::sizing` for numeric,
/// array, and alignment logic.
fn datum_width(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::Numeric(d) => sizing::numeric_datum_width(d),
        Value::Date(_) => 4,
        Value::Time(_) | Value::Timestamp(_) => 8,
        Value::Interval { .. } => 16,
        Value::Uuid(_) => 16,
        Value::Text(s) => s.len() + 4,
        Value::Json(s) => s.len() + 4,
        Value::Jsonb(s) => sizing::jsonb_datum_width(s),
        Value::Bytes(b) => b.len() + 4,
        Value::Tsvector(s) => sizing::tsvector_datum_width(s),
        Value::Tsquery(s) => sizing::tsquery_datum_width(s),
        Value::Vector(v) => v.len() * 4 + 4, // f32 per dim + header
        Value::Array(a) => sizing::array_datum_width(a),
    }
}

// ── Column accumulator ──────────────────────────────────────────────────

/// Per-column aggregation state during streaming scan.  Only counters and
/// a distinct-value map are held in memory — never full row vectors.
struct ColumnAccumulator {
    null_count: usize,
    non_null_count: usize,
    total_width: usize,
    /// Canonical key bytes → (representative Value, frequency count).
    /// The representative Value is stored on first observation — no
    /// deserialization needed at finalization time.
    /// Only contains values ≤ WIDTH_THRESHOLD bytes.
    value_counts: HashMap<Vec<u8>, (Value, usize)>,
    /// Count of over-width non-NULL values observed. Each observation is
    /// assumed distinct (matches PG analyze.c: "Overwidth values are assumed
    /// to have been distinct"). This avoids storing wide key bytes in memory.
    wide_value_count: usize,
}

impl ColumnAccumulator {
    fn new() -> Self {
        Self {
            null_count: 0,
            non_null_count: 0,
            total_width: 0,
            value_counts: HashMap::new(),
            wide_value_count: 0,
        }
    }

    fn observe(&mut self, value: &Value) {
        if matches!(value, Value::Null) {
            self.null_count += 1;
        } else {
            self.non_null_count += 1;
            let key_bytes = encode_value_key(value);
            self.total_width += key_bytes.len();
            // Skip wide values from MCV/histogram collection (matches PG's
            // WIDTH_THRESHOLD on datum width, not encoding width).
            // Wide values are still counted for null_fraction, n_distinct,
            // and avg_width.
            if datum_width(value) <= WIDTH_THRESHOLD {
                self.value_counts
                    .entry(key_bytes)
                    .and_modify(|(_val, count)| *count += 1)
                    .or_insert_with(|| (canonicalize_value(value), 1));
            } else {
                // Each wide observation assumed distinct (matches PG analyze.c).
                self.wide_value_count += 1;
            }
        }
    }
}

// ── Statistics computation ──────────────────────────────────────────────

/// Build `ColumnStatistics` from a finalized accumulator.
fn finalize_column(acc: &ColumnAccumulator, row_count: usize) -> ColumnStatistics {
    if row_count == 0 {
        return ColumnStatistics::empty();
    }

    let null_fraction = acc.null_count as f64 / row_count as f64;
    // NDV = narrow distinct values (in value_counts) + wide value observations.
    // Wide values are excluded from MCVs/histograms but each observation is
    // assumed distinct (matches PG: "Overwidth values are assumed to have been
    // distinct"). This may over-count if wide values repeat, but matches PG.
    let n_distinct = (acc.value_counts.len() + acc.wide_value_count) as f64;
    let avg_width = acc.total_width.checked_div(acc.non_null_count).unwrap_or(0);

    // MCV: top values by frequency, tie-broken by canonical key bytes ascending.
    let mut entries: Vec<(&Vec<u8>, &(Value, usize))> = acc.value_counts.iter().collect();
    entries.sort_by(|a, b| {
        b.1 .1
            .cmp(&a.1 .1) // descending frequency
            .then_with(|| a.0.cmp(b.0)) // ascending key bytes for determinism
    });

    let mcv_count = entries.len().min(MCV_LIMIT);
    let mcv_entries = &entries[..mcv_count];

    let mut most_common_vals = Vec::with_capacity(mcv_count);
    let mut most_common_freqs = Vec::with_capacity(mcv_count);
    let mcv_keys: HashSet<&Vec<u8>> = mcv_entries.iter().map(|(k, _)| *k).collect();

    for (_, (val, count)) in mcv_entries {
        most_common_vals.push(val.clone());
        most_common_freqs.push(*count as f64 / row_count as f64);
    }

    // Histogram: equi-depth bounds from non-MCV values.
    let histogram_bounds = build_histogram(&acc.value_counts, &mcv_keys, row_count);

    ColumnStatistics {
        null_fraction,
        n_distinct,
        avg_width,
        most_common_vals,
        most_common_freqs,
        histogram_bounds,
        correlation: 0.0, // deferred — computing requires O(N) per-column state
    }
}

/// Build equi-depth histogram bounds from non-MCV value entries.
///
/// Returns empty Vec when:
/// - No non-MCV values remain after MCV exclusion
/// - The column's values are not orderable (compare_values fails)
fn build_histogram(
    value_counts: &HashMap<Vec<u8>, (Value, usize)>,
    mcv_keys: &HashSet<&Vec<u8>>,
    _row_count: usize,
) -> Vec<Value> {
    // Collect non-MCV entries: (Value, frequency).
    let mut non_mcv: Vec<(Value, usize)> = Vec::new();
    for (key_bytes, (val, count)) in value_counts {
        if mcv_keys.contains(key_bytes) {
            continue;
        }
        non_mcv.push((val.clone(), *count));
    }

    if non_mcv.is_empty() {
        return Vec::new();
    }

    // Test orderability with actual values (no hardcoded type blacklist).
    if non_mcv.len() >= 2 && compare_values(&non_mcv[0].0, &non_mcv[1].0).is_err() {
        return Vec::new(); // unorderable type — skip histogram
    }

    // Sort by value using compare_values.
    non_mcv.sort_by(|a, b| match compare_values(&a.0, &b.0) {
        Ok(ord) => {
            if ord < 0 {
                std::cmp::Ordering::Less
            } else if ord > 0 {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        }
        Err(_) => std::cmp::Ordering::Equal,
    });

    // When distinct non-MCV values <= bucket count, return all sorted.
    if non_mcv.len() <= HISTOGRAM_BUCKETS {
        return non_mcv.into_iter().map(|(v, _)| v).collect();
    }

    // Weighted equi-depth: cumulative row count determines bucket boundaries.
    let total_weight: usize = non_mcv.iter().map(|(_, c)| c).sum();
    let bucket_weight = total_weight as f64 / HISTOGRAM_BUCKETS as f64;
    let mut bounds = Vec::with_capacity(HISTOGRAM_BUCKETS);
    let mut cumulative = 0usize;
    let mut next_boundary = bucket_weight;

    for (i, (val, count)) in non_mcv.iter().enumerate() {
        cumulative += count;
        // Place a boundary when cumulative weight crosses the next threshold,
        // or at the very last entry.
        if cumulative as f64 >= next_boundary || i == non_mcv.len() - 1 {
            bounds.push(val.clone());
            next_boundary += bucket_weight;
            if bounds.len() >= HISTOGRAM_BUCKETS {
                break;
            }
        }
    }

    bounds
}

// ── Streaming table analysis ────────────────────────────────────────────

/// Analyze a single table, producing statistics via streaming batch scan.
async fn analyze_table(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<TableStatistics> {
    let num_cols = schema.columns.len();
    let mut accumulators: Vec<ColumnAccumulator> =
        (0..num_cols).map(|_| ColumnAccumulator::new()).collect();

    let row_count = store
        .scan_analyze_batch(txn, db_id, schema.table_id, ANALYZE_BATCH_SIZE, |row| {
            for (i, value) in row.values.iter().enumerate() {
                if i < accumulators.len() {
                    accumulators[i].observe(value);
                }
            }
            Ok(())
        })
        .await?;

    let mut columns = HashMap::new();
    for (i, acc) in accumulators.iter().enumerate() {
        if let Some(col_def) = schema.columns.get(i) {
            let col_stats = finalize_column(acc, row_count);
            columns.insert(col_def.name.clone(), col_stats);
        }
    }

    Ok(TableStatistics {
        table_id: schema.table_id,
        row_count,
        last_analyzed: statement_time::statement_timestamp_millis_or_now(),
        columns,
    })
}

// ── Executor integration ────────────────────────────────────────────────

/// Schemas to skip during bare ANALYZE (system catalogs).
const SKIP_SCHEMAS: &[&str] = &["information_schema", "pg_catalog", "extensions"];
const ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS: usize = 10;

enum AnalyzeAutocommitTableOutcome {
    Skip,
    Applied {
        table_id: u64,
        stats: TableStatistics,
    },
}

enum AnalyzeAttemptFailureDisposition {
    Retry,
    ReturnErr(anyhow::Error),
}

fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.downcast_ref::<SqlError>()
        .is_some_and(|e| matches!(e, SqlError::PermissionDenied { .. }))
}

fn should_skip_schema(full_name: &str) -> bool {
    let schema_name = full_name.split('.').next().unwrap_or("");
    SKIP_SCHEMAS.contains(&schema_name)
}

fn should_retry_analyze_autocommit_attempt(attempt: usize, retryable: bool) -> bool {
    attempt + 1 < ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS && retryable
}

fn decide_analyze_attempt_failure(
    attempt: usize,
    err: anyhow::Error,
    rollback_result: Result<()>,
    retryable: bool,
) -> AnalyzeAttemptFailureDisposition {
    if let Err(rollback_err) = rollback_result {
        return AnalyzeAttemptFailureDisposition::ReturnErr(anyhow!(
            "{}; rollback failed: {}",
            err,
            rollback_err
        ));
    }
    if should_retry_analyze_autocommit_attempt(attempt, retryable) {
        AnalyzeAttemptFailureDisposition::Retry
    } else {
        AnalyzeAttemptFailureDisposition::ReturnErr(err)
    }
}

impl Executor {
    async fn analyze_single_table_in_current_txn(
        &self,
        session: &mut Session,
        db_id: u64,
        current_role: Option<&str>,
        obj_name: &sqlparser::ast::ObjectName,
    ) -> Result<()> {
        let (txn, _sequence_values, search_path) = session
            .get_mut_txn_sequence_values_and_search_path()
            .expect("Transaction must be active");

        let resolved = names::resolve_existing_table_name(
            self.store.as_ref(),
            txn,
            db_id,
            obj_name,
            search_path,
        )
        .await?
        .ok_or_else(|| anyhow!("relation \"{}\" does not exist", obj_name))?;

        self.require_table_privilege(txn, current_role, Privilege::Select, &resolved.full)
            .await?;

        let schema = self
            .store
            .get_schema(txn, db_id, &resolved.full)
            .await?
            .ok_or_else(|| anyhow!("relation \"{}\" does not exist", resolved.full))?;

        let stats = analyze_table(&self.store, txn, db_id, &schema).await?;
        self.store.store_statistics(txn, db_id, &stats).await?;
        self.stats_cache()
            .update_full_stats(db_id, schema.table_id, Arc::new(stats));
        self.stats_cache().reset_mod_count(db_id, schema.table_id);
        Ok(())
    }

    async fn analyze_all_tables_in_current_txn(
        &self,
        session: &mut Session,
        db_id: u64,
        current_role: Option<&str>,
    ) -> Result<()> {
        let (txn, _sequence_values, _search_path) = session
            .get_mut_txn_sequence_values_and_search_path()
            .expect("Transaction must be active");

        let all_tables = self.store.list_tables(txn, db_id).await?;
        for full_name in &all_tables {
            if should_skip_schema(full_name) {
                continue;
            }

            if let Err(err) = self
                .require_table_privilege(txn, current_role, Privilege::Select, full_name)
                .await
            {
                if is_permission_denied(&err) {
                    continue;
                }
                return Err(err);
            }

            let schema = match self.store.get_schema(txn, db_id, full_name).await? {
                Some(s) => s,
                None => continue,
            };

            let stats = analyze_table(&self.store, txn, db_id, &schema).await?;
            self.store.store_statistics(txn, db_id, &stats).await?;
            self.stats_cache()
                .update_full_stats(db_id, schema.table_id, Arc::new(stats));
            self.stats_cache().reset_mod_count(db_id, schema.table_id);
        }
        Ok(())
    }

    async fn rollback_autocommit_attempt_with_error(
        &self,
        session: &mut Session,
        attempt: usize,
        err: anyhow::Error,
    ) -> Result<()> {
        let retryable = is_retryable_tikv_error(&err);
        let rollback_res = session.rollback().await;
        self.clear_trigger_activations();
        match decide_analyze_attempt_failure(attempt, err, rollback_res, retryable) {
            AnalyzeAttemptFailureDisposition::Retry => {
                autocommit_backoff(attempt).await;
                Ok(())
            }
            AnalyzeAttemptFailureDisposition::ReturnErr(err) => Err(err),
        }
    }

    async fn discover_bare_analyze_tables_autocommit(
        &self,
        session: &mut Session,
        db_id: u64,
    ) -> Result<Vec<String>> {
        for attempt in 0..ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS {
            session.begin().await?;
            let discovery_result = async {
                let (txn, _sequence_values, _search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let tables = self.store.list_tables(txn, db_id).await?;
                let candidates = tables
                    .into_iter()
                    .filter(|full_name| !should_skip_schema(full_name))
                    .collect::<Vec<_>>();
                Ok::<Vec<String>, anyhow::Error>(candidates)
            }
            .await;

            match discovery_result {
                Ok(candidates) => match session.commit().await {
                    Ok(_) => {
                        self.flush_trigger_activations();
                        self.flush_pending_hnsw_merges();
                        self.flush_pending_storage_dirty();
                        return Ok(candidates);
                    }
                    Err(err) => {
                        self.rollback_autocommit_attempt_with_error(session, attempt, err)
                            .await?;
                        continue;
                    }
                },
                Err(err) => {
                    self.rollback_autocommit_attempt_with_error(session, attempt, err)
                        .await?;
                    continue;
                }
            }
        }
        unreachable!("ANALYZE discovery retry loop must return")
    }

    async fn analyze_table_autocommit_with_retry(
        &self,
        session: &mut Session,
        db_id: u64,
        current_role: Option<&str>,
        full_name: &str,
    ) -> Result<()> {
        for attempt in 0..ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS {
            session.begin().await?;
            let attempt_result = async {
                let (txn, _sequence_values, _search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                if let Err(err) = self
                    .require_table_privilege(txn, current_role, Privilege::Select, full_name)
                    .await
                {
                    if is_permission_denied(&err) {
                        return Ok(AnalyzeAutocommitTableOutcome::Skip);
                    }
                    return Err(err);
                }

                let schema = match self.store.get_schema(txn, db_id, full_name).await? {
                    Some(s) => s,
                    None => return Ok(AnalyzeAutocommitTableOutcome::Skip),
                };

                let stats = analyze_table(&self.store, txn, db_id, &schema).await?;
                self.store.store_statistics(txn, db_id, &stats).await?;
                Ok::<AnalyzeAutocommitTableOutcome, anyhow::Error>(
                    AnalyzeAutocommitTableOutcome::Applied {
                        table_id: schema.table_id,
                        stats,
                    },
                )
            }
            .await;

            match attempt_result {
                Ok(AnalyzeAutocommitTableOutcome::Skip) => {
                    let rollback_res = session.rollback().await;
                    self.clear_trigger_activations();
                    return match rollback_res {
                        Ok(_) => Ok(()),
                        Err(err) => Err(err),
                    };
                }
                Ok(AnalyzeAutocommitTableOutcome::Applied { table_id, stats }) => {
                    match session.commit().await {
                        Ok(_) => {
                            self.flush_trigger_activations();
                            self.flush_pending_hnsw_merges();
                            self.flush_pending_storage_dirty();
                            self.stats_cache()
                                .update_full_stats(db_id, table_id, Arc::new(stats));
                            self.stats_cache().reset_mod_count(db_id, table_id);
                            return Ok(());
                        }
                        Err(err) => {
                            self.rollback_autocommit_attempt_with_error(session, attempt, err)
                                .await?;
                            continue;
                        }
                    }
                }
                Err(err) => {
                    self.rollback_autocommit_attempt_with_error(session, attempt, err)
                        .await?;
                    continue;
                }
            }
        }
        unreachable!("ANALYZE per-table retry loop must return")
    }

    pub(crate) async fn execute_analyze_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let table_name = parse_analyze_table_name(sql)?;
        let is_autocommit = !session.is_in_transaction();
        let current_role = session.current_user().map(|u| u.to_string());
        let db_id = session.current_database_id();

        // Bare ANALYZE in autocommit mode: discover a fixed candidate set,
        // then process each table in its own autocommit transaction. Missing
        // tables at execution time are skipped.
        if table_name.is_none() && is_autocommit {
            let candidates = self
                .discover_bare_analyze_tables_autocommit(session, db_id)
                .await?;
            for full_name in &candidates {
                self.analyze_table_autocommit_with_retry(
                    session,
                    db_id,
                    current_role.as_deref(),
                    full_name,
                )
                .await?;
            }
            return Ok(ExecuteResult::CommandComplete { tag: "ANALYZE" });
        }

        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            match &table_name {
                Some(obj_name) => {
                    self.analyze_single_table_in_current_txn(
                        session,
                        db_id,
                        current_role.as_deref(),
                        obj_name,
                    )
                    .await?;
                }
                None => {
                    self.analyze_all_tables_in_current_txn(session, db_id, current_role.as_deref())
                        .await?;
                }
            }

            Ok(ExecuteResult::CommandComplete { tag: "ANALYZE" })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
                self.flush_trigger_activations();
                self.flush_pending_hnsw_merges();
                self.flush_pending_storage_dirty();
            } else {
                session.rollback().await?;
                self.clear_trigger_activations();
            }
        }

        result
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::sql::optimizer::statistics::ColumnStatistics;

    /// Helper: build a ColumnAccumulator from a slice of Values, then finalize.
    fn stats_from_values(values: &[Value]) -> ColumnStatistics {
        let mut acc = ColumnAccumulator::new();
        for v in values {
            acc.observe(v);
        }
        finalize_column(&acc, values.len())
    }

    // ── Classification tests (in raw_sql.rs test module, added here for gating) ──

    #[test]
    fn classify_analyze_bare() {
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE"),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    #[test]
    fn classify_analyze_with_table() {
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE USERS"),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    #[test]
    fn classify_analyze_verbose() {
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE VERBOSE USERS"),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    #[test]
    fn classify_analyze_no_word_boundary() {
        assert_eq!(crate::sql::raw_sql::classify("ANALYZEFOO"), None);
    }

    #[test]
    fn classify_analyze_quoted() {
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE \"MYTABLE\""),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    #[test]
    fn classify_analyze_invalid_tail_routes_to_handler() {
        // Classifier routes; handler returns syntax error.
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE FOO BAR"),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    #[test]
    fn classify_analyze_trailing_comment() {
        assert_eq!(
            crate::sql::raw_sql::classify("ANALYZE -- COMMENT"),
            Some(crate::sql::raw_sql::RawSqlKind::Analyze)
        );
    }

    // ── Table name parsing ──

    #[test]
    fn parse_bare_analyze() {
        assert!(parse_analyze_table_name("ANALYZE").unwrap().is_none());
    }

    #[test]
    fn parse_analyze_single_table() {
        let name = parse_analyze_table_name("ANALYZE users").unwrap().unwrap();
        assert_eq!(name.to_string(), "users");
    }

    #[test]
    fn parse_analyze_qualified_table() {
        let name = parse_analyze_table_name("ANALYZE public.users")
            .unwrap()
            .unwrap();
        assert_eq!(name.to_string(), "public.users");
    }

    #[test]
    fn parse_analyze_verbose_table() {
        let name = parse_analyze_table_name("ANALYZE VERBOSE users")
            .unwrap()
            .unwrap();
        assert_eq!(name.to_string(), "users");
    }

    #[test]
    fn parse_analyze_quoted_table() {
        let name = parse_analyze_table_name("ANALYZE \"MyTable\"")
            .unwrap()
            .unwrap();
        // sqlparser preserves the quoted identifier.
        assert!(name.to_string().contains("MyTable"));
    }

    #[test]
    fn parse_analyze_quoted_schema_table() {
        let name = parse_analyze_table_name("ANALYZE \"s\".\"t\"")
            .unwrap()
            .unwrap();
        let s = name.to_string();
        assert!(s.contains("s") && s.contains("t"));
    }

    #[test]
    fn parse_analyze_trailing_where_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users WHERE id > 1").is_err());
    }

    #[test]
    fn parse_analyze_alias_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users u").is_err());
    }

    // Dispatch passes sql_trimmed (leading comments/whitespace already stripped),
    // so parse_analyze_table_name always sees "ANALYZE..." at byte 0.  Verify
    // that the function works correctly when called with clean input.
    #[test]
    fn parse_analyze_assumes_trimmed_input() {
        // The dispatcher strips leading whitespace/comments before calling us.
        // Verify the happy path works with the trimmed form.
        let name = parse_analyze_table_name("ANALYZE users").unwrap().unwrap();
        assert_eq!(name.to_string(), "users");
        // Raw leading-space input would be wrong — that's the dispatcher's job.
        // "  ANALYZE users" is not a valid call to parse_analyze_table_name.
    }

    // P0: unterminated block comment must be a syntax error, not bare ANALYZE.
    #[test]
    fn parse_analyze_unterminated_comment_is_error() {
        assert!(parse_analyze_table_name("ANALYZE /*").is_err());
        assert!(parse_analyze_table_name("ANALYZE VERBOSE /*").is_err());
    }

    // P1: Query-level trailing clauses (ORDER BY, LIMIT, OFFSET, FOR UPDATE)
    // must be rejected.
    #[test]
    fn parse_analyze_trailing_order_by_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users ORDER BY 1").is_err());
    }

    #[test]
    fn parse_analyze_trailing_limit_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users LIMIT 1").is_err());
    }

    #[test]
    fn parse_analyze_trailing_offset_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users OFFSET 0").is_err());
    }

    #[test]
    fn parse_analyze_trailing_for_update_rejected() {
        assert!(parse_analyze_table_name("ANALYZE users FOR UPDATE").is_err());
    }

    // ── Column statistics computation ──

    #[test]
    fn stats_empty_rows() {
        let stats = stats_from_values(&[]);
        assert_eq!(stats.null_fraction, 0.0);
        assert_eq!(stats.n_distinct, 0.0);
        assert!(stats.most_common_vals.is_empty());
        assert!(stats.histogram_bounds.is_empty());
        assert_eq!(stats.correlation, 0.0);
    }

    #[test]
    fn stats_all_null() {
        let values: Vec<Value> = (0..50).map(|_| Value::Null).collect();
        let stats = stats_from_values(&values);
        assert_eq!(stats.null_fraction, 1.0);
        assert_eq!(stats.n_distinct, 0.0);
        assert!(stats.most_common_vals.is_empty());
        assert!(stats.histogram_bounds.is_empty());
    }

    #[test]
    fn stats_known_int_distribution() {
        let values: Vec<Value> = (1..=100).map(Value::Int32).collect();
        let stats = stats_from_values(&values);
        assert_eq!(stats.null_fraction, 0.0);
        assert_eq!(stats.n_distinct, 100.0);
        // 100 distinct values, 10 MCVs → 90 non-MCV → histogram should have bounds
        assert!(!stats.histogram_bounds.is_empty());
        assert_eq!(stats.correlation, 0.0);
    }

    #[test]
    fn stats_mcv_skewed() {
        // Skewed: value 1 appears 50 times, values 2..=20 appear once each.
        let mut values = Vec::new();
        for _ in 0..50 {
            values.push(Value::Int32(1));
        }
        for i in 2..=20 {
            values.push(Value::Int32(i));
        }
        let stats = stats_from_values(&values);
        // Value 1 should be the top MCV.
        assert!(!stats.most_common_vals.is_empty());
        assert_eq!(stats.most_common_vals[0], Value::Int32(1));
        // Its frequency should be 50/69.
        assert!((stats.most_common_freqs[0] - 50.0 / 69.0).abs() < 0.001);
    }

    #[test]
    fn stats_mixed_nulls() {
        let mut values = Vec::new();
        for i in 0..50 {
            values.push(Value::Int32(i));
        }
        for _ in 0..50 {
            values.push(Value::Null);
        }
        let stats = stats_from_values(&values);
        assert!((stats.null_fraction - 0.5).abs() < 0.01);
        assert_eq!(stats.n_distinct, 50.0);
    }

    #[test]
    fn stats_unorderable_json_skips_histogram() {
        // Json values should cause compare_values to fail → empty histogram.
        let values = vec![
            Value::Json(r#"{"a":1}"#.to_string()),
            Value::Json(r#"{"b":2}"#.to_string()),
            Value::Json(r#"{"c":3}"#.to_string()),
        ];
        let stats = stats_from_values(&values);
        assert!(stats.histogram_bounds.is_empty());
    }

    #[test]
    fn stats_key_canonicalization_neg_zero() {
        // 0.0 and -0.0 should be treated as the same distinct value.
        let values = vec![
            Value::Float64(0.0),
            Value::Float64(-0.0),
            Value::Float64(1.0),
        ];
        let stats = stats_from_values(&values);
        // n_distinct should be 2 (0.0 and 1.0), not 3.
        assert_eq!(stats.n_distinct, 2.0);
    }

    // ── Gate tests ──

    #[test]
    fn gate1_invalid_tail_rejected_at_handler() {
        // Alias rejected.
        assert!(parse_analyze_table_name("ANALYZE foo bar").is_err());
        // Quoted identifier accepted.
        assert!(parse_analyze_table_name("ANALYZE \"MyTable\"").is_ok());
        // Classifier only enforces word boundary.
        assert_eq!(crate::sql::raw_sql::classify("ANALYZEFOO"), None);
    }

    #[test]
    fn gate2_bare_analyze_only_skips_permission_denied() {
        // Construct SqlError::PermissionDenied → should be downcastable.
        let err: anyhow::Error = SqlError::PermissionDenied {
            object_type: "table".to_string(),
            object_name: "secret".to_string(),
        }
        .into();
        assert!(is_permission_denied(&err));

        // Non-SqlError → should NOT match PermissionDenied.
        let err2: anyhow::Error = anyhow!("TiKV connection lost");
        assert!(!is_permission_denied(&err2));
    }

    #[test]
    fn bare_analyze_schema_filter_skips_system_catalogs_only() {
        assert!(should_skip_schema("information_schema.tables"));
        assert!(should_skip_schema("pg_catalog.pg_class"));
        assert!(should_skip_schema("extensions.pg_trgm"));
        assert!(!should_skip_schema("public.users"));
    }

    #[test]
    fn analyze_retry_gate_respects_attempt_budget_and_retryable_flag() {
        assert!(should_retry_analyze_autocommit_attempt(0, true));
        assert!(should_retry_analyze_autocommit_attempt(
            ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS - 2,
            true
        ));
        assert!(!should_retry_analyze_autocommit_attempt(
            ANALYZE_AUTOCOMMIT_MAX_ATTEMPTS - 1,
            true
        ));
        assert!(!should_retry_analyze_autocommit_attempt(0, false));
    }

    #[test]
    fn analyze_attempt_failure_decision_retries_on_retryable_after_successful_rollback() {
        let decision = decide_analyze_attempt_failure(0, anyhow!("write conflict"), Ok(()), true);
        assert!(matches!(decision, AnalyzeAttemptFailureDisposition::Retry));
    }

    #[test]
    fn analyze_attempt_failure_decision_returns_error_when_not_retryable() {
        let decision =
            decide_analyze_attempt_failure(0, anyhow!("permanent failure"), Ok(()), false);
        match decision {
            AnalyzeAttemptFailureDisposition::Retry => panic!("expected return error"),
            AnalyzeAttemptFailureDisposition::ReturnErr(err) => {
                assert!(err.to_string().contains("permanent failure"));
            }
        }
    }

    #[test]
    fn analyze_attempt_failure_decision_surfaces_rollback_failure() {
        let decision = decide_analyze_attempt_failure(
            0,
            anyhow!("commit failed"),
            Err(anyhow!("rollback failed")),
            true,
        );
        match decision {
            AnalyzeAttemptFailureDisposition::Retry => panic!("expected return error"),
            AnalyzeAttemptFailureDisposition::ReturnErr(err) => {
                let msg = err.to_string();
                assert!(msg.contains("commit failed"));
                assert!(msg.contains("rollback failed"));
            }
        }
    }

    #[test]
    fn gate3_histogram_degenerate_cases() {
        // (a) 3 distinct non-MCV values, 100 buckets → all 3 sorted.
        // Use 13 distinct values total so that MCV takes top 10, leaving 3.
        let mut values = Vec::new();
        // Top 10 MCVs: values 1..=10 with high frequency.
        for i in 1..=10 {
            for _ in 0..20 {
                values.push(Value::Int32(i));
            }
        }
        // 3 non-MCV values with frequency 1 each.
        values.push(Value::Int32(100));
        values.push(Value::Int32(200));
        values.push(Value::Int32(300));
        let stats = stats_from_values(&values);
        // Histogram should contain exactly 3 values (100, 200, 300) sorted.
        assert_eq!(stats.histogram_bounds.len(), 3);
        assert_eq!(stats.histogram_bounds[0], Value::Int32(100));
        assert_eq!(stats.histogram_bounds[1], Value::Int32(200));
        assert_eq!(stats.histogram_bounds[2], Value::Int32(300));

        // (b) All values are MCV (<=10 distinct) → histogram empty.
        let values_b: Vec<Value> = (1..=5)
            .flat_map(|i| std::iter::repeat_n(Value::Int32(i), 10))
            .collect();
        let stats_b = stats_from_values(&values_b);
        assert!(stats_b.histogram_bounds.is_empty());

        // (c) Weighted depth: values {1→1000, 2→1, 3→1}.
        // All 3 are MCVs (<=10 distinct), so histogram is empty.
        // Test with >10 distinct to exercise weighted depth.
        let mut values_c = Vec::new();
        // 1 heavy value
        for _ in 0..1000 {
            values_c.push(Value::Int32(1));
        }
        // 11 light values so MCV takes top 10, leaving some for histogram
        for i in 2..=12 {
            values_c.push(Value::Int32(i * 100));
        }
        let stats_c = stats_from_values(&values_c);
        // Value 1 (freq 1000) is MCV. The remaining 11 light values: MCV takes
        // top 10 by frequency (each freq=1, tie-broken by key). The one left
        // over is in the histogram.
        // With 11 light values (freq 1 each) + 1 heavy (freq 1000), MCV takes
        // the top 10 by frequency. Value 1 is always top. Then 10 of the 11
        // light values. 1 light value remains → histogram has 1 entry.
        assert!(!stats_c.histogram_bounds.is_empty());
    }

    #[test]
    fn gate4_unorderable_by_evidence_not_blacklist() {
        // Json values → compare_values fails → histogram skipped.
        assert!(compare_values(
            &Value::Json(r#"{"a":1}"#.to_string()),
            &Value::Json(r#"{"b":2}"#.to_string())
        )
        .is_err());

        // Int32 array values → test actual comparability.
        let arr1 = Value::Array(vec![Value::Int32(1)]);
        let arr2 = Value::Array(vec![Value::Int32(2)]);
        let cmp_result = compare_values(&arr1, &arr2);
        // If compare_values succeeds for arrays, histogram IS built
        // (need >10 distinct values so some go beyond MCV into histogram).
        if cmp_result.is_ok() {
            let mut values: Vec<Value> = Vec::new();
            // Create 15 distinct array values; top 10 by frequency become MCVs,
            // remaining 5 go to histogram.
            for i in 1..=10 {
                for _ in 0..5 {
                    values.push(Value::Array(vec![Value::Int32(i)]));
                }
            }
            for i in 11..=15 {
                values.push(Value::Array(vec![Value::Int32(i)]));
            }
            let stats = stats_from_values(&values);
            assert!(
                !stats.histogram_bounds.is_empty(),
                "histogram should be built when compare_values succeeds"
            );
        }
        // If compare_values fails for arrays, histogram is skipped.
        if cmp_result.is_err() {
            let values = vec![arr1, arr2, Value::Array(vec![Value::Int32(3)])];
            let stats = stats_from_values(&values);
            assert!(
                stats.histogram_bounds.is_empty(),
                "histogram should be empty when compare_values fails"
            );
        }
    }
}
