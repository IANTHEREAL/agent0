//! Table and column statistics for cost-based optimization.
//!
//! Modeled after PostgreSQL's `pg_statistic` catalog. Statistics are collected
//! by `ANALYZE` and used by the physical planner to produce accurate cost
//! estimates (row counts, selectivities).

use crate::model::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Statistics for an entire table, collected by `ANALYZE`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStatistics {
    /// Internal table ID (from `TableSchema::table_id`).
    pub table_id: u64,
    /// Estimated total row count at analyze time.
    pub row_count: usize,
    /// Unix timestamp (milliseconds) when `ANALYZE` last ran.
    pub last_analyzed: i64,
    /// Per-column statistics, keyed by column name.
    pub columns: HashMap<String, ColumnStatistics>,
}

/// Per-column statistics, following PostgreSQL `pg_statistic` conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStatistics {
    /// Fraction of rows that are NULL, in `[0.0, 1.0]`.
    pub null_fraction: f64,
    /// Number of distinct non-NULL values.
    ///
    /// PostgreSQL convention:
    /// - Positive: absolute count of distinct values.
    /// - Negative: fraction of rows that are distinct (e.g. −1.0 = all unique).
    pub n_distinct: f64,
    /// Average width in bytes of the column's serialized representation.
    pub avg_width: usize,
    /// Most common non-NULL values, ordered by descending frequency.
    pub most_common_vals: Vec<Value>,
    /// Frequencies corresponding to `most_common_vals`, in the same order.
    pub most_common_freqs: Vec<f64>,
    /// Equi-depth histogram bounds for non-NULL, non-MCV values.
    ///
    /// Empty for unorderable types (JSON, arrays).
    pub histogram_bounds: Vec<Value>,
    /// Correlation between physical row order and logical sort order,
    /// in `[-1.0, 1.0]`. 1.0 means perfectly ordered, −1.0 means
    /// perfectly reverse-ordered.
    pub correlation: f64,
}

impl ColumnStatistics {
    /// Returns an empty `ColumnStatistics` with all fields at their zero/default values.
    pub fn empty() -> Self {
        Self {
            null_fraction: 0.0,
            n_distinct: 0.0,
            avg_width: 0,
            most_common_vals: Vec::new(),
            most_common_freqs: Vec::new(),
            histogram_bounds: Vec::new(),
            correlation: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_statistics_empty_defaults() {
        let cs = ColumnStatistics::empty();
        assert_eq!(cs.null_fraction, 0.0);
        assert_eq!(cs.n_distinct, 0.0);
        assert_eq!(cs.avg_width, 0);
        assert!(cs.most_common_vals.is_empty());
        assert!(cs.most_common_freqs.is_empty());
        assert!(cs.histogram_bounds.is_empty());
        assert_eq!(cs.correlation, 0.0);
    }

    #[test]
    fn test_table_statistics_serde_roundtrip() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.0,
                n_distinct: 1000.0,
                avg_width: 4,
                most_common_vals: vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)],
                most_common_freqs: vec![0.01, 0.008, 0.005],
                histogram_bounds: vec![Value::Int32(10), Value::Int32(500), Value::Int32(999)],
                correlation: 0.98,
            },
        );
        columns.insert(
            "name".to_string(),
            ColumnStatistics {
                null_fraction: 0.05,
                n_distinct: -0.8,
                avg_width: 12,
                most_common_vals: vec![
                    Value::Text("alice".to_string()),
                    Value::Text("bob".to_string()),
                ],
                most_common_freqs: vec![0.1, 0.05],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );

        let stats = TableStatistics {
            table_id: 42,
            row_count: 10000,
            last_analyzed: 1708100000000,
            columns,
        };

        // bincode round-trip
        let encoded = bincode::serialize(&stats).expect("serialize");
        let decoded: TableStatistics = bincode::deserialize(&encoded).expect("deserialize");

        assert_eq!(decoded.table_id, 42);
        assert_eq!(decoded.row_count, 10000);
        assert_eq!(decoded.last_analyzed, 1708100000000);
        assert_eq!(decoded.columns.len(), 2);

        let id_stats = &decoded.columns["id"];
        assert_eq!(id_stats.null_fraction, 0.0);
        assert_eq!(id_stats.n_distinct, 1000.0);
        assert_eq!(id_stats.avg_width, 4);
        assert_eq!(id_stats.most_common_vals.len(), 3);
        assert_eq!(id_stats.most_common_freqs.len(), 3);
        assert_eq!(id_stats.histogram_bounds.len(), 3);
        assert!((id_stats.correlation - 0.98).abs() < f64::EPSILON);

        let name_stats = &decoded.columns["name"];
        assert_eq!(name_stats.null_fraction, 0.05);
        assert_eq!(name_stats.n_distinct, -0.8);
        assert_eq!(name_stats.most_common_vals.len(), 2);
    }

    #[test]
    fn test_table_statistics_empty_columns_roundtrip() {
        let stats = TableStatistics {
            table_id: 1,
            row_count: 0,
            last_analyzed: 0,
            columns: HashMap::new(),
        };

        let encoded = bincode::serialize(&stats).expect("serialize");
        let decoded: TableStatistics = bincode::deserialize(&encoded).expect("deserialize");

        assert_eq!(decoded.table_id, 1);
        assert_eq!(decoded.row_count, 0);
        assert!(decoded.columns.is_empty());
    }

    #[test]
    fn test_column_statistics_with_nulls_and_special_values() {
        let cs = ColumnStatistics {
            null_fraction: 1.0,
            n_distinct: 0.0,
            avg_width: 0,
            most_common_vals: vec![Value::Null],
            most_common_freqs: vec![1.0],
            histogram_bounds: vec![],
            correlation: 0.0,
        };

        let encoded = bincode::serialize(&cs).expect("serialize");
        let decoded: ColumnStatistics = bincode::deserialize(&encoded).expect("deserialize");
        assert_eq!(decoded.null_fraction, 1.0);
        assert_eq!(decoded.most_common_vals, vec![Value::Null]);
    }
}
