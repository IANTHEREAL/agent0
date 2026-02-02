#![allow(dead_code)]

use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::Expr;
use std::collections::HashMap;

pub trait EvalContext {
    fn row(&self) -> Option<&Row>;

    fn resolve_column(&self, name: &str) -> Result<Value>;

    fn resolve_compound_identifier(&self, parts: &[sqlparser::ast::Ident]) -> Result<Value>;

    fn schema(&self) -> Option<&TableSchema>;

    fn is_timestamptz(&self, expr: &Expr) -> bool;

    fn column_type(&self, expr: &Expr) -> Option<&DataType>;
}

pub struct SingleTableContext<'a> {
    row: Option<&'a Row>,
    schema: Option<&'a TableSchema>,
}

impl<'a> SingleTableContext<'a> {
    pub fn new(row: Option<&'a Row>, schema: Option<&'a TableSchema>) -> Self {
        Self { row, schema }
    }

    pub fn empty() -> Self {
        Self {
            row: None,
            schema: None,
        }
    }

    pub fn row(&self) -> Option<&Row> {
        self.row
    }

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
                let idx = schema
                    .column_index(name)
                    .ok_or_else(|| anyhow!("Column '{}' not found", name))?;
                Ok(row.values[idx].clone())
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
                    let qualified_name = format!("{}.{}", table_part, col_name);
                    let idx = schema
                        .column_index(&qualified_name)
                        .or_else(|| schema.column_index(col_name))
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
}

pub struct JoinEvalContext<'a> {
    pub column_offsets: &'a HashMap<String, usize>,
    pub merged_column_offsets: Option<&'a HashMap<String, Vec<usize>>>,
    pub combined_row: &'a Row,
    pub combined_schema: &'a TableSchema,
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
        }
    }

    pub fn from_join_context(ctx: &'a super::JoinContext<'a>) -> Self {
        Self {
            column_offsets: ctx.column_offsets,
            merged_column_offsets: ctx.merged_column_offsets,
            combined_row: ctx.combined_row,
            combined_schema: ctx.combined_schema,
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
}
