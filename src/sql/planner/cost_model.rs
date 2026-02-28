//! Cost model constants for query planning and index selection
//!
//! This module contains all hardcoded constants used in cost estimation
//! and selectivity calculation for the query planner.

/// Cost model constants for index selection and query planning
pub struct CostModel;

impl CostModel {
    /// Base cost for an index scan operation (fixed overhead)
    pub const BASE_COST: f64 = 1.0;

    /// Cost per row for index scan operations
    pub const ROW_COST: f64 = 0.5;

    /// Selectivity estimate for two-sided range queries (e.g., col BETWEEN a AND b)
    pub const TWO_SIDED_RANGE_SELECTIVITY: f64 = 0.1;

    /// Selectivity estimate for one-sided range queries (e.g., col > a or col < b)
    pub const ONE_SIDED_RANGE_SELECTIVITY: f64 = 0.3;

    /// Selectivity estimate for unique index exact matches
    pub const UNIQUE_INDEX_SELECTIVITY: f64 = 1.0 / 1000000.0;

    /// Base selectivity factor for non-unique index matches (raised to the power of matched columns)
    pub const NON_UNIQUE_SELECTIVITY_BASE: f64 = 0.1;

    /// Minimum selectivity floor - no query is estimated to match fewer than this fraction of rows
    pub const MIN_SELECTIVITY_FLOOR: f64 = 0.0001;

    /// Selectivity threshold above which full table scan is preferred over index scan.
    /// When an index scan would fetch more than this fraction of table rows, the random
    /// I/O overhead outweighs the benefit of reading fewer rows sequentially.
    pub const FULL_SCAN_SELECTIVITY_THRESHOLD: f64 = 0.3;

    /// Per-row cost for index scans that exceed the selectivity threshold.
    /// Reflects random I/O overhead (cf. PostgreSQL's `random_page_cost = 4.0`).
    /// With this value, index cost = 1 + 4.0 * estimated_rows, which exceeds
    /// full-scan cost (= table_rows) when selectivity > ~25%.
    pub const RANDOM_IO_COST_PER_ROW: f64 = 4.0;

    // ── GIN index scan cost constants ──────────────────────────

    /// Base (fixed) cost of a GIN index scan, higher than B-tree because
    /// multiple posting-list range scans + set operations are required.
    pub const GIN_SCAN_BASE_COST: f64 = 4.0;

    /// Per-token scan cost: each distinct token hash requires one TiKV
    /// range scan over the posting list.
    pub const GIN_TOKEN_SCAN_COST: f64 = 1.0;

    /// Per-row fetch cost for GIN candidate rows (batch_get after posting
    /// list intersection/union).
    pub const GIN_ROW_FETCH_COST: f64 = 1.0;

    /// Default GIN selectivity when no column statistics are available.
    /// Assumes ~1% of rows match a typical FTS / containment query.
    pub const GIN_DEFAULT_SELECTIVITY: f64 = 0.01;

    // ── HNSW index scan cost constants ──────────────────────────

    /// Base cost of HNSW scan: graph load (amortized) + beam search.
    pub const HNSW_SCAN_BASE_COST: f64 = 2.0;

    /// Per-result cost: distance computation per candidate.
    pub const HNSW_PER_RESULT_COST: f64 = 0.1;

    #[allow(dead_code)]
    /// Default HNSW selectivity when no statistics are available.
    pub const HNSW_DEFAULT_SELECTIVITY: f64 = 0.001;
}
