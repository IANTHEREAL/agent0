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
}
