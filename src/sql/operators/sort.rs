use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::pool::{try_grow_statement_memory_scope, try_shrink_statement_memory_scope};
use crate::sql::analyzer::types::TypedOrderByExpr;
use crate::sql::collation::ResolvedCollation;
use crate::sql::expr::collation_aware::extract_resolved_collation;
use crate::sql::expr::compare_order_by_values_collated;
use crate::sql::expr::operators::{sort_by_fallible, JsonbSortKey};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::memory::{estimate_row_size, estimate_value_size};

fn enforce_sort_memory_limit(
    total_bytes: &mut usize,
    row: &Row,
    max_sort_bytes: usize,
) -> Result<()> {
    if max_sort_bytes == 0 {
        return Ok(());
    }

    *total_bytes = total_bytes.saturating_add(estimate_row_size(row));
    if *total_bytes > max_sort_bytes {
        return Err(anyhow!(
            "ORDER BY sort memory limit exceeded: estimated {} bytes exceeds db9.max_sort_bytes={} bytes. Reduce result set with WHERE/LIMIT or increase db9.max_sort_bytes",
            *total_bytes,
            max_sort_bytes
        ));
    }

    Ok(())
}

/// Sort key with pre-parsed expensive-to-compare types.
/// NULL is its own variant so nulls_first/nulls_last is always respected.
#[derive(Debug, Clone)]
enum SortKey {
    /// SQL NULL from any column type.
    Null,
    /// Pre-parsed JSONB — compared via Ord (no re-parse).
    Jsonb(JsonbSortKey),
    /// All other types — compared via compare_order_by_values_collated().
    Plain(Value),
}

fn estimate_sort_keys_size(keys: &[SortKey]) -> usize {
    std::mem::size_of::<Vec<SortKey>>()
        + keys
            .iter()
            .map(|k| match k {
                SortKey::Null => std::mem::size_of::<SortKey>(),
                SortKey::Plain(v) => std::mem::size_of::<SortKey>() + estimate_value_size(v),
                SortKey::Jsonb(k) => {
                    std::mem::size_of::<SortKey>()
                        + std::mem::size_of::<JsonbSortKey>()
                        + k.estimate_heap_size()
                }
            })
            .sum::<usize>()
}

#[derive(Debug)]
pub struct SortOperator {
    child: BoxedOperator,
    order_by: Vec<TypedOrderByExpr>,
    /// Resolved collation per ORDER BY key (extracted at construction time).
    collations: Vec<Option<ResolvedCollation>>,
    sorted_rows: Vec<Row>,
    sorted_rows_charged_bytes: usize,
    position: usize,
    opened: bool,
}

impl SortOperator {
    pub fn new(child: BoxedOperator, order_by: Vec<TypedOrderByExpr>) -> Self {
        let collations: Vec<Option<ResolvedCollation>> = order_by
            .iter()
            .map(|ob| extract_resolved_collation(&ob.expr))
            .collect();
        Self {
            child,
            order_by,
            collations,
            sorted_rows: Vec::new(),
            sorted_rows_charged_bytes: 0,
            position: 0,
            opened: false,
        }
    }

    fn compute_sort_keys(&self, row: &Row, ctx: &mut ExecutionContext<'_>) -> Result<Vec<SortKey>> {
        let mut keys = Vec::with_capacity(self.order_by.len());
        for order_expr in &self.order_by {
            let value = eval_typed_expr(&order_expr.expr, row, ctx.query_ctx)?;
            let key = match &value {
                Value::Null => SortKey::Null,
                Value::Jsonb(s) => SortKey::Jsonb(JsonbSortKey::from_str(s)?),
                _ => SortKey::Plain(value),
            };
            keys.push(key);
        }
        Ok(keys)
    }

    fn compare_keys(&self, keys_a: &[SortKey], keys_b: &[SortKey]) -> Result<std::cmp::Ordering> {
        for (i, order_expr) in self.order_by.iter().enumerate() {
            let asc = order_expr.asc;
            let nulls_first = order_expr.nulls_first;
            let collation = self.collations[i].as_ref();

            let ordering = match (&keys_a[i], &keys_b[i]) {
                // ── NULL handling (mirrors compare_order_by_values_collated) ──
                (SortKey::Null, SortKey::Null) => std::cmp::Ordering::Equal,
                (SortKey::Null, _) => {
                    if nulls_first {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                (_, SortKey::Null) => {
                    if nulls_first {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                }

                // ── Pre-parsed JSONB (no re-parse) ──
                (SortKey::Jsonb(a), SortKey::Jsonb(b)) => {
                    let cmp = a.cmp(b);
                    if asc {
                        cmp
                    } else {
                        cmp.reverse()
                    }
                }

                // ── Standard values (delegate to existing comparator) ──
                (SortKey::Plain(a), SortKey::Plain(b)) => {
                    compare_order_by_values_collated(a, b, asc, nulls_first, collation)?
                }

                // Same ORDER BY column produces same SortKey variant for all
                // non-NULL rows (type is fixed per column). This arm is
                // structurally unreachable.
                _ => {
                    return Err(anyhow!(
                        "BUG: mixed SortKey variants in compare_keys (JSONB vs non-JSONB)"
                    ));
                }
            };
            if ordering != std::cmp::Ordering::Equal {
                return Ok(ordering);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }
}

#[async_trait]
impl PhysicalOperator for SortOperator {
    fn schema(&self) -> &TableSchema {
        self.child.schema()
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        if self.sorted_rows_charged_bytes > 0 {
            try_shrink_statement_memory_scope(self.sorted_rows_charged_bytes);
            self.sorted_rows_charged_bytes = 0;
        }
        self.sorted_rows.clear();

        self.child.open(ctx).await?;

        let max_sort_bytes = crate::session_context::current_max_sort_bytes();
        let mut total_bytes = 0usize;
        let mut rows_charged_bytes = 0usize;
        let mut keyed_rows_charged_bytes = 0usize;
        let mut rows = Vec::new();
        while let Some(row) = self.child.next(ctx).await? {
            enforce_sort_memory_limit(&mut total_bytes, &row, max_sort_bytes)?;
            let row_bytes = estimate_row_size(&row);
            try_grow_statement_memory_scope("operators.sort.rows", row_bytes)?;
            rows_charged_bytes = rows_charged_bytes.saturating_add(row_bytes);
            rows.push(row);
        }
        let mut keyed_rows: Vec<(Vec<SortKey>, Row)> = Vec::with_capacity(rows.len());
        for row in rows {
            let keys = self.compute_sort_keys(&row, ctx)?;
            let keyed_row_bytes =
                estimate_sort_keys_size(&keys) + std::mem::size_of::<(Vec<SortKey>, Row)>();
            try_grow_statement_memory_scope("operators.sort.keyed_rows", keyed_row_bytes)?;
            keyed_rows_charged_bytes = keyed_rows_charged_bytes.saturating_add(keyed_row_bytes);
            keyed_rows.push((keys, row));
        }
        sort_by_fallible(&mut keyed_rows, |(keys_a, _), (keys_b, _)| {
            self.compare_keys(keys_a, keys_b)
        })?;
        let rows = keyed_rows.into_iter().map(|(_, row)| row).collect();

        // Runtime scope for keyed rows ends after extraction into owned sorted rows.
        try_shrink_statement_memory_scope(keyed_rows_charged_bytes);

        self.sorted_rows = rows;
        self.sorted_rows_charged_bytes = rows_charged_bytes;
        self.position = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.sorted_rows.len() {
            let row = self.sorted_rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        let child_close_result = self.child.close(ctx).await;
        try_shrink_statement_memory_scope(self.sorted_rows_charged_bytes);
        self.sorted_rows_charged_bytes = 0;
        self.sorted_rows.clear();
        self.opened = false;
        child_close_result
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.child.as_mut()]
    }

    fn name(&self) -> &'static str {
        "Sort"
    }

    fn explain_info(&self) -> Option<String> {
        let keys: Vec<String> = self
            .order_by
            .iter()
            .map(|o| {
                let dir = if o.asc { "ASC" } else { "DESC" };
                format!("{:?} {}", o.expr, dir)
            })
            .collect();
        Some(format!("order_by=[{}]", keys.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType};
    use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};

    fn test_schema() -> TableSchema {
        TableSchema::new(
            "test".to_string(),
            1,
            vec![
                ColumnDef::new("id", DataType::Int32, false).primary_key(),
                ColumnDef::new("name", DataType::Text, true),
            ],
            vec![0],
        )
    }

    fn typed_col_ref(name: &str, index: usize, data_type: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type,
        }
    }

    #[test]
    fn test_sort_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];

        let sort = SortOperator::new(child, order_by);

        assert_eq!(sort.name(), "Sort");
        assert!(!sort.opened);
    }

    #[test]
    fn test_sort_explain_info() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![
            TypedOrderByExpr {
                expr: typed_col_ref("id", 0, DataType::Int32),
                asc: true,
                nulls_first: false,
            },
            TypedOrderByExpr {
                expr: typed_col_ref("name", 1, DataType::Text),
                asc: false,
                nulls_first: true,
            },
        ];

        let sort = SortOperator::new(child, order_by);

        let info = sort.explain_info().unwrap();
        assert!(info.contains("ASC"));
        assert!(info.contains("DESC"));
    }

    #[test]
    fn test_compare_rows() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];

        let sort = SortOperator::new(child, order_by);

        let keys1 = vec![SortKey::Plain(Value::Int32(1))];
        let keys2 = vec![SortKey::Plain(Value::Int32(2))];

        assert_eq!(
            sort.compare_keys(&keys1, &keys2).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&keys2, &keys1).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&keys1, &keys1).unwrap(),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_compare_keys_desc_ordering() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: false,
            nulls_first: true,
        }];

        let sort = SortOperator::new(child, order_by);

        let keys1 = vec![SortKey::Plain(Value::Int32(1))];
        let keys2 = vec![SortKey::Plain(Value::Int32(2))];

        assert_eq!(
            sort.compare_keys(&keys1, &keys2).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&keys2, &keys1).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_keys_null_handling() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();

        // ASC + NULLS FIRST: NULL < non-NULL
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: true,
        }];
        let sort = SortOperator::new(child, order_by);

        let null_key = vec![SortKey::Null];
        let val_key = vec![SortKey::Plain(Value::Int32(1))];

        assert_eq!(
            sort.compare_keys(&null_key, &val_key).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&val_key, &null_key).unwrap(),
            std::cmp::Ordering::Greater
        );

        // ASC + NULLS LAST: NULL > non-NULL
        let child = Box::new(TableScanOperator::new(schema));
        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];
        let sort = SortOperator::new(child, order_by);

        assert_eq!(
            sort.compare_keys(&null_key, &val_key).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&val_key, &null_key).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_keys_multi_column_tiebreak() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![
            TypedOrderByExpr {
                expr: typed_col_ref("name", 1, DataType::Text),
                asc: true,
                nulls_first: false,
            },
            TypedOrderByExpr {
                expr: typed_col_ref("id", 0, DataType::Int32),
                asc: false,
                nulls_first: true,
            },
        ];

        let sort = SortOperator::new(child, order_by);

        // Same first key, tiebreak by second key (DESC)
        let keys_a = vec![
            SortKey::Plain(Value::Text("Alice".to_string())),
            SortKey::Plain(Value::Int32(1)),
        ];
        let keys_b = vec![
            SortKey::Plain(Value::Text("Alice".to_string())),
            SortKey::Plain(Value::Int32(2)),
        ];

        // Second column is DESC, so 2 comes before 1
        assert_eq!(
            sort.compare_keys(&keys_a, &keys_b).unwrap(),
            std::cmp::Ordering::Greater
        );

        // Different first key — tiebreak not needed
        let keys_c = vec![
            SortKey::Plain(Value::Text("Bob".to_string())),
            SortKey::Plain(Value::Int32(1)),
        ];
        assert_eq!(
            sort.compare_keys(&keys_a, &keys_c).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_estimated_row_size_includes_payload() {
        let small = Row::new(vec![Value::Int32(1), Value::Text("a".to_string())]);
        let big = Row::new(vec![
            Value::Int32(1),
            Value::Text("a".repeat(1024)),
            Value::Json("{\"k\":\"v\"}".repeat(128)),
        ]);

        assert!(estimate_row_size(&big) > estimate_row_size(&small));
    }

    #[test]
    fn test_sort_memory_limit_within_limit() {
        let row = Row::new(vec![Value::Text("abc".to_string())]);
        let mut total_bytes = 0usize;
        let limit = estimate_row_size(&row) + 1;

        enforce_sort_memory_limit(&mut total_bytes, &row, limit).unwrap();
        assert!(total_bytes <= limit);
    }

    #[test]
    fn test_sort_memory_limit_exceeded() {
        let row = Row::new(vec![Value::Text("x".repeat(1024))]);
        let mut total_bytes = 0usize;
        let limit = estimate_row_size(&row) - 1;

        let err = enforce_sort_memory_limit(&mut total_bytes, &row, limit).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ORDER BY sort memory limit exceeded"));
        assert!(msg.contains("db9.max_sort_bytes"));
    }

    #[test]
    fn test_sort_memory_limit_zero_is_unlimited() {
        let row = Row::new(vec![Value::Text("x".repeat(2048))]);
        let mut total_bytes = 0usize;

        enforce_sort_memory_limit(&mut total_bytes, &row, 0).unwrap();
        assert_eq!(total_bytes, 0);
    }

    #[test]
    fn test_compare_keys_jsonb_pre_parsed() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];

        let sort = SortOperator::new(child, order_by);

        let k1 = vec![SortKey::Jsonb(JsonbSortKey::from_str("1").unwrap())];
        let k2 = vec![SortKey::Jsonb(JsonbSortKey::from_str("2").unwrap())];
        let k_str = vec![SortKey::Jsonb(
            JsonbSortKey::from_str(r#""hello""#).unwrap(),
        )];

        // ASC: 1 < 2
        assert_eq!(
            sort.compare_keys(&k1, &k2).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&k2, &k1).unwrap(),
            std::cmp::Ordering::Greater
        );
        // PG: string < number
        assert_eq!(
            sort.compare_keys(&k_str, &k1).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_keys_jsonb_desc() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: false,
            nulls_first: true,
        }];

        let sort = SortOperator::new(child, order_by);

        let k1 = vec![SortKey::Jsonb(JsonbSortKey::from_str("1").unwrap())];
        let k2 = vec![SortKey::Jsonb(JsonbSortKey::from_str("2").unwrap())];

        // DESC: 1 > 2 in output order
        assert_eq!(
            sort.compare_keys(&k1, &k2).unwrap(),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_compare_keys_null_vs_jsonb_nulls_first() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();

        // NULLS FIRST: NULL sorts before JSONB
        let child = Box::new(TableScanOperator::new(schema.clone()));
        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: true,
        }];
        let sort = SortOperator::new(child, order_by);

        let null_key = vec![SortKey::Null];
        let jsonb_key = vec![SortKey::Jsonb(JsonbSortKey::from_str("42").unwrap())];

        assert_eq!(
            sort.compare_keys(&null_key, &jsonb_key).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort.compare_keys(&jsonb_key, &null_key).unwrap(),
            std::cmp::Ordering::Greater
        );

        // NULLS LAST: NULL sorts after JSONB
        let child = Box::new(TableScanOperator::new(schema));
        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];
        let sort = SortOperator::new(child, order_by);

        assert_eq!(
            sort.compare_keys(&null_key, &jsonb_key).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            sort.compare_keys(&jsonb_key, &null_key).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_keys_null_vs_null() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: true,
        }];
        let sort = SortOperator::new(child, order_by);

        let null_a = vec![SortKey::Null];
        let null_b = vec![SortKey::Null];

        assert_eq!(
            sort.compare_keys(&null_a, &null_b).unwrap(),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_compare_keys_mixed_variant_errors() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let order_by = vec![TypedOrderByExpr {
            expr: typed_col_ref("id", 0, DataType::Int32),
            asc: true,
            nulls_first: false,
        }];
        let sort = SortOperator::new(child, order_by);

        let plain_key = vec![SortKey::Plain(Value::Int32(1))];
        let jsonb_key = vec![SortKey::Jsonb(JsonbSortKey::from_str("42").unwrap())];

        let err = sort
            .compare_keys(&plain_key, &jsonb_key)
            .unwrap_err()
            .to_string();
        assert!(err.contains("BUG: mixed SortKey variants"));
    }

    #[test]
    fn test_jsonb_sort_key_size_scales_with_payload() {
        let small = vec![SortKey::Jsonb(JsonbSortKey::from_str("42").unwrap())];
        let large = vec![SortKey::Jsonb(
            JsonbSortKey::from_str(&format!(r#"{{"key": "{}"}}"#, "x".repeat(1000))).unwrap(),
        )];

        let small_size = estimate_sort_keys_size(&small);
        let large_size = estimate_sort_keys_size(&large);

        // Large JSONB must report substantially more memory than small
        assert!(
            large_size > small_size + 500,
            "JSONB sort key size must scale with payload: small={small_size}, large={large_size}"
        );
    }

    #[test]
    fn test_jsonb_sort_parse_count() {
        use crate::sql::expr::operators::test_counters;

        let n = 10usize;

        // Phase 1: materializing N sort keys parses exactly N times
        test_counters::reset();
        let sort_keys: Vec<JsonbSortKey> = (0..n)
            .map(|i| JsonbSortKey::from_str(&format!("{}", i)).unwrap())
            .collect();
        assert_eq!(test_counters::get(), n);

        // Phase 2: O(N) comparisons trigger zero additional parses
        let pre_cmp = test_counters::get();
        for i in 0..n - 1 {
            let _ = sort_keys[i].cmp(&sort_keys[i + 1]);
        }
        assert_eq!(test_counters::get(), pre_cmp); // zero additional parses
    }
}
