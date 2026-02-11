use crate::sql::query_context::QueryContext;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::Expr;
use std::collections::HashMap;

fn is_hidden_subquery_column(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, col)| col)
        .unwrap_or(name)
        .starts_with("__tipg_subquery_")
}

fn composite_field_needs_quotes(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace())
}

fn format_composite_field(value: &Value) -> String {
    let field_str = match value {
        Value::Null => return String::new(),
        Value::Text(s) => s.clone(),
        other => other.to_string(),
    };

    if !composite_field_needs_quotes(&field_str) {
        return field_str;
    }

    let mut out = String::with_capacity(field_str.len() + 2);
    out.push('"');
    for ch in field_str.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn format_composite_value(values: impl IntoIterator<Item = Value>) -> Value {
    let mut fields = String::new();
    for (idx, v) in values.into_iter().enumerate() {
        if idx > 0 {
            fields.push(',');
        }
        fields.push_str(&format_composite_field(&v));
    }
    Value::Text(format!("({})", fields))
}

pub trait EvalContext {
    fn row(&self) -> Option<&Row>;

    fn resolve_column(&self, name: &str) -> Result<Value>;

    fn resolve_compound_identifier(&self, parts: &[sqlparser::ast::Ident]) -> Result<Value>;

    fn schema(&self) -> Option<&TableSchema>;

    fn is_timestamptz(&self, expr: &Expr) -> bool;

    fn column_type(&self, expr: &Expr) -> Option<&DataType>;

    fn query_context(&self) -> Option<&QueryContext> {
        None
    }
}

pub struct SingleTableContext<'a> {
    row: Option<&'a Row>,
    schema: Option<&'a TableSchema>,
    query_ctx: Option<&'a QueryContext>,
}

impl<'a> SingleTableContext<'a> {
    pub fn new(row: Option<&'a Row>, schema: Option<&'a TableSchema>) -> Self {
        Self {
            row,
            schema,
            query_ctx: None,
        }
    }

    pub fn with_query_ctx(
        row: Option<&'a Row>,
        schema: Option<&'a TableSchema>,
        query_ctx: &'a QueryContext,
    ) -> Self {
        Self {
            row,
            schema,
            query_ctx: Some(query_ctx),
        }
    }

    #[allow(dead_code)] // eval context API
    pub fn empty() -> Self {
        Self {
            row: None,
            schema: None,
            query_ctx: None,
        }
    }

    #[allow(dead_code)] // eval context API
    pub fn row(&self) -> Option<&Row> {
        self.row
    }

    #[allow(dead_code)] // eval context API
    pub fn get_schema(&self) -> Option<&TableSchema> {
        self.schema
    }
}

impl EvalContext for SingleTableContext<'_> {
    fn row(&self) -> Option<&Row> {
        self.row
    }

    fn resolve_column(&self, name: &str) -> Result<Value> {
        if name.eq_ignore_ascii_case("DEFAULT") {
            return Ok(Value::Null);
        }

        match (self.row, self.schema) {
            (Some(row), Some(schema)) => {
                if let Some(idx) = schema.column_index(name) {
                    return Ok(row.values[idx].clone());
                }

                let name_lower = name.to_lowercase();
                let prefix = format!("{}.", name_lower);
                let prefixed_indices: Vec<usize> = schema
                    .columns
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, col)| {
                        if col.name.to_lowercase().starts_with(&prefix)
                            && !is_hidden_subquery_column(&col.name)
                        {
                            Some(idx)
                        } else {
                            None
                        }
                    })
                    .collect();

                if !prefixed_indices.is_empty() {
                    let values = prefixed_indices
                        .into_iter()
                        .filter_map(|idx| row.values.get(idx).cloned())
                        .collect::<Vec<_>>();
                    return Ok(format_composite_value(values));
                }

                // Whole-row reference: check FROM alias first, then schema name
                let is_alias_match = schema
                    .from_alias
                    .as_ref()
                    .map_or(false, |a| a.eq_ignore_ascii_case(name));
                let short_name_matches = schema
                    .name
                    .rsplit('.')
                    .next()
                    .is_some_and(|short| short.eq_ignore_ascii_case(name));
                if is_alias_match || short_name_matches {
                    let values = schema
                        .columns
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, col)| {
                            if is_hidden_subquery_column(&col.name) {
                                None
                            } else {
                                row.values.get(idx).cloned()
                            }
                        })
                        .collect::<Vec<_>>();
                    return Ok(format_composite_value(values));
                }

                Err(anyhow!("Column '{}' not found", name))
            }
            _ => Err(anyhow!(
                "Cannot evaluate identifier '{}' without row context",
                name
            )),
        }
    }

    fn resolve_compound_identifier(&self, parts: &[sqlparser::ast::Ident]) -> Result<Value> {
        if parts.len() >= 2 {
            let table_part = &parts[parts.len() - 2].value;
            let col_name = &parts[parts.len() - 1].value;

            match (self.row, self.schema) {
                (Some(row), Some(schema)) => {
                    // Try exact qualified name first (e.g., "t.id" column in schema)
                    let qualified_name = format!("{}.{}", table_part, col_name);
                    if let Some(idx) = schema.column_index(&qualified_name) {
                        return Ok(row.values[idx].clone());
                    }
                    // If the table_part matches our FROM alias or the schema name,
                    // resolve the unqualified column name
                    let alias_match = schema
                        .from_alias
                        .as_ref()
                        .map_or(false, |a| a.eq_ignore_ascii_case(table_part));
                    let schema_short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                    if alias_match || schema_short.eq_ignore_ascii_case(table_part) {
                        if let Some(idx) = schema.column_index(col_name) {
                            return Ok(row.values[idx].clone());
                        }
                    }
                    // Fallback: try unqualified column name regardless
                    let idx = schema
                        .column_index(col_name)
                        .ok_or_else(|| anyhow!("Column '{}' not found", col_name))?;
                    Ok(row.values[idx].clone())
                }
                _ => Err(anyhow!("Cannot evaluate column without row context")),
            }
        } else {
            Err(anyhow!("Invalid compound identifier"))
        }
    }

    fn schema(&self) -> Option<&TableSchema> {
        self.schema
    }

    fn is_timestamptz(&self, expr: &Expr) -> bool {
        super::expr_is_timestamptz(expr, self.schema)
    }

    fn column_type(&self, expr: &Expr) -> Option<&DataType> {
        match expr {
            Expr::Identifier(ident) => self.schema.and_then(|s| {
                s.column_index(&ident.value)
                    .map(|idx| &s.columns[idx].data_type)
            }),
            Expr::CompoundIdentifier(parts) => parts.last().and_then(|ident| {
                self.schema.and_then(|s| {
                    s.column_index(&ident.value)
                        .map(|idx| &s.columns[idx].data_type)
                })
            }),
            _ => None,
        }
    }

    fn query_context(&self) -> Option<&QueryContext> {
        self.query_ctx
    }
}

pub struct JoinEvalContext<'a> {
    pub column_offsets: &'a HashMap<String, usize>,
    pub merged_column_offsets: Option<&'a HashMap<String, Vec<usize>>>,
    pub combined_row: &'a Row,
    pub combined_schema: &'a TableSchema,
    pub query_ctx: Option<&'a QueryContext>,
}

impl<'a> JoinEvalContext<'a> {
    pub fn new(
        column_offsets: &'a HashMap<String, usize>,
        merged_column_offsets: Option<&'a HashMap<String, Vec<usize>>>,
        combined_row: &'a Row,
        combined_schema: &'a TableSchema,
    ) -> Self {
        Self {
            column_offsets,
            merged_column_offsets,
            combined_row,
            combined_schema,
            query_ctx: None,
        }
    }
}

impl EvalContext for JoinEvalContext<'_> {
    fn row(&self) -> Option<&Row> {
        Some(self.combined_row)
    }

    fn resolve_column(&self, name: &str) -> Result<Value> {
        if let Some(merged) = self.merged_column_offsets {
            if let Some(offsets) = merged.get(name) {
                for &offset in offsets {
                    if let Some(val) = self.combined_row.values.get(offset) {
                        if !matches!(val, Value::Null) {
                            return Ok(val.clone());
                        }
                    }
                }
                return Ok(Value::Null);
            }

            // Case-insensitive fallback to match the behavior used for qualified identifiers.
            let name_lower = name.to_lowercase();
            for (k, offsets) in merged {
                if k.to_lowercase() == name_lower {
                    for &offset in offsets {
                        if let Some(val) = self.combined_row.values.get(offset) {
                            if !matches!(val, Value::Null) {
                                return Ok(val.clone());
                            }
                        }
                    }
                    return Ok(Value::Null);
                }
            }
        }

        if let Some(&offset) = self.column_offsets.get(name) {
            return Ok(self.combined_row.values[offset].clone());
        }

        // If an unqualified identifier doesn't exist in the short-name map,
        // it may be ambiguous (multiple joined inputs expose the same column name).
        let mut first_offset: Option<usize> = None;
        for (key, &offset) in self.column_offsets {
            if let Some((_, suffix)) = key.rsplit_once('.') {
                if suffix == name {
                    match first_offset {
                        None => first_offset = Some(offset),
                        Some(prev) if prev != offset => {
                            return Err(anyhow!("column reference \"{}\" is ambiguous", name));
                        }
                        Some(_) => {}
                    }
                }
            }
        }

        Err(anyhow!("Column '{}' not found or ambiguous", name))
    }

    fn resolve_compound_identifier(&self, parts: &[sqlparser::ast::Ident]) -> Result<Value> {
        let (table_alias, col_name) = if parts.len() == 2 {
            (&parts[0].value, &parts[1].value)
        } else if parts.len() == 3 {
            (&parts[1].value, &parts[2].value)
        } else {
            return Err(anyhow!(
                "Unsupported compound identifier with {} parts",
                parts.len()
            ));
        };

        let key = format!("{}.{}", table_alias, col_name);
        if let Some(&offset) = self.column_offsets.get(&key) {
            return Ok(self.combined_row.values[offset].clone());
        }

        let key_lower = format!("{}.{}", table_alias.to_lowercase(), col_name.to_lowercase());
        for (k, &offset) in self.column_offsets {
            if k.to_lowercase() == key_lower {
                return Ok(self.combined_row.values[offset].clone());
            }
        }

        if table_alias.contains("->") {
            let suffix = format!(".{}.{}", table_alias, col_name);
            let suffix_lower = suffix.to_lowercase();
            for (k, &offset) in self.column_offsets {
                if k.to_lowercase().ends_with(&suffix_lower) {
                    return Ok(self.combined_row.values[offset].clone());
                }
            }
            let direct_suffix = format!("{}.{}", table_alias, col_name);
            let direct_suffix_lower = direct_suffix.to_lowercase();
            for (k, &offset) in self.column_offsets {
                if k.to_lowercase() == direct_suffix_lower
                    || k.to_lowercase()
                        .ends_with(&format!(".{}", direct_suffix_lower))
                {
                    return Ok(self.combined_row.values[offset].clone());
                }
            }
        }

        Err(anyhow!("Column '{}.{}' not found", table_alias, col_name))
    }

    fn schema(&self) -> Option<&TableSchema> {
        Some(self.combined_schema)
    }

    fn is_timestamptz(&self, expr: &Expr) -> bool {
        super::expr_is_timestamptz_join_with_schema(expr, self.column_offsets, self.combined_schema)
    }

    fn column_type(&self, expr: &Expr) -> Option<&DataType> {
        match expr {
            Expr::Identifier(ident) => self
                .column_offsets
                .get(&ident.value)
                .and_then(|&offset| self.combined_schema.columns.get(offset))
                .map(|col| &col.data_type),
            Expr::CompoundIdentifier(parts) => {
                if parts.len() != 2 {
                    return None;
                }
                let table_alias = &parts[0].value;
                let col_name = &parts[1].value;

                let mut key = String::with_capacity(table_alias.len() + 1 + col_name.len());
                key.push_str(table_alias);
                key.push('.');
                key.push_str(col_name);

                if let Some(&offset) = self.column_offsets.get(key.as_str()) {
                    return self
                        .combined_schema
                        .columns
                        .get(offset)
                        .map(|col| &col.data_type);
                }

                for (k, &offset) in self.column_offsets {
                    if k.eq_ignore_ascii_case(&key) {
                        return self
                            .combined_schema
                            .columns
                            .get(offset)
                            .map(|col| &col.data_type);
                    }
                }

                None
            }
            _ => None,
        }
    }

    fn query_context(&self) -> Option<&QueryContext> {
        self.query_ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
    use std::collections::HashMap;

    fn int_col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        }
    }

    fn schema(cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            columns: cols,
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_column_unqualified_ambiguous_errors() {
        let mut column_offsets: HashMap<String, usize> = HashMap::new();
        column_offsets.insert("a.id".to_string(), 0);
        column_offsets.insert("b.id".to_string(), 1);

        let combined_row = Row::new(vec![Value::Int32(1), Value::Int32(2)]);
        let combined_schema = schema(vec![int_col("id"), int_col("id")]);

        let ctx = JoinEvalContext::new(&column_offsets, None, &combined_row, &combined_schema);
        let err = ctx.resolve_column("id").unwrap_err().to_string();
        assert!(err.contains("column reference \"id\" is ambiguous"));
    }

    #[test]
    fn test_resolve_column_whole_row_reference_by_schema_name() {
        let schema = TableSchema {
            name: "foo".to_string(),
            columns: vec![int_col("i"), int_col("mod2")],
            ..Default::default()
        };
        let row = Row::new(vec![Value::Int32(5), Value::Int32(1)]);
        let ctx = SingleTableContext::new(Some(&row), Some(&schema));
        assert_eq!(
            ctx.resolve_column("foo").unwrap(),
            Value::Text("(5,1)".to_string())
        );
    }

    #[test]
    fn test_resolve_column_whole_row_reference_by_prefix() {
        let schema = TableSchema {
            name: "join_result".to_string(),
            columns: vec![int_col("foo.a"), int_col("foo.b"), int_col("bar.c")],
            ..Default::default()
        };
        let row = Row::new(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        let ctx = SingleTableContext::new(Some(&row), Some(&schema));
        assert_eq!(
            ctx.resolve_column("foo").unwrap(),
            Value::Text("(1,2)".to_string())
        );
    }

    #[test]
    fn test_resolve_column_whole_row_skips_hidden_subquery_columns() {
        let schema = TableSchema {
            name: "foo".to_string(),
            columns: vec![int_col("i"), int_col("__tipg_subquery_0")],
            ..Default::default()
        };
        let row = Row::new(vec![Value::Int32(1), Value::Int32(999)]);
        let ctx = SingleTableContext::new(Some(&row), Some(&schema));
        assert_eq!(
            ctx.resolve_column("foo").unwrap(),
            Value::Text("(1)".to_string())
        );
    }

    #[test]
    fn test_resolve_column_whole_row_reference_by_from_alias() {
        let schema = TableSchema {
            name: "public.foo_tbl".to_string(),
            columns: vec![int_col("i"), int_col("mod2")],
            from_alias: Some("bar".to_string()),
            ..Default::default()
        };
        let row = Row::new(vec![Value::Int32(1), Value::Int32(2)]);
        let ctx = SingleTableContext::new(Some(&row), Some(&schema));
        // Resolving by alias should work
        assert_eq!(
            ctx.resolve_column("bar").unwrap(),
            Value::Text("(1,2)".to_string())
        );
        // Resolving by original table name should also still work
        assert_eq!(
            ctx.resolve_column("foo_tbl").unwrap(),
            Value::Text("(1,2)".to_string())
        );
    }

    #[test]
    fn test_resolve_compound_identifier_by_from_alias() {
        let schema = TableSchema {
            name: "public.foo_tbl".to_string(),
            columns: vec![int_col("i"), int_col("mod2")],
            from_alias: Some("bar".to_string()),
            ..Default::default()
        };
        let row = Row::new(vec![Value::Int32(1), Value::Int32(2)]);
        let ctx = SingleTableContext::new(Some(&row), Some(&schema));
        let parts = vec![
            sqlparser::ast::Ident::new("bar"),
            sqlparser::ast::Ident::new("i"),
        ];
        // bar.i should resolve to the i column value
        assert_eq!(
            ctx.resolve_compound_identifier(&parts).unwrap(),
            Value::Int32(1)
        );
    }
}
