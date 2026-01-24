//! Type context for multi-table column resolution

use std::collections::HashMap;

use crate::types::{ColumnDef, TableSchema};

use super::error::TypeError;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedColumn<'a> {
    pub table_alias: Option<&'a str>,
    pub column_index: usize,
    pub column_def: &'a ColumnDef,
}

pub struct TypeContext<'a> {
    tables: HashMap<&'a str, &'a TableSchema>,
    column_index: HashMap<String, Vec<(&'a str, usize)>>,
    single_table: Option<&'a TableSchema>,
}

impl<'a> TypeContext<'a> {
    pub fn single(schema: &'a TableSchema) -> Self {
        let mut ctx = Self {
            tables: HashMap::new(),
            column_index: HashMap::new(),
            single_table: Some(schema),
        };
        ctx.add_table("", schema);
        ctx
    }

    pub fn empty() -> Self {
        Self {
            tables: HashMap::new(),
            column_index: HashMap::new(),
            single_table: None,
        }
    }

    pub fn add_table(&mut self, alias: &'a str, schema: &'a TableSchema) {
        self.tables.insert(alias, schema);
        for (idx, col) in schema.columns.iter().enumerate() {
            let lower_name = col.name.to_lowercase();
            self.column_index
                .entry(lower_name)
                .or_default()
                .push((alias, idx));
        }
        if self.tables.len() > 1 {
            self.single_table = None;
        }
    }

    pub fn join(
        left_alias: &'a str,
        left: &'a TableSchema,
        right_alias: &'a str,
        right: &'a TableSchema,
    ) -> Self {
        let mut ctx = Self::empty();
        ctx.add_table(left_alias, left);
        ctx.add_table(right_alias, right);
        ctx
    }

    pub fn resolve_column(&self, name: &str) -> Result<ResolvedColumn<'a>, TypeError> {
        let lower = name.to_lowercase();

        // Fast path: single table mode
        if let Some(schema) = self.single_table {
            for (idx, col) in schema.columns.iter().enumerate() {
                if col.name.eq_ignore_ascii_case(name) {
                    return Ok(ResolvedColumn {
                        table_alias: None,
                        column_index: idx,
                        column_def: col,
                    });
                }
            }
            return Err(TypeError::ColumnNotFound {
                name: name.to_string(),
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
            });
        }

        // Multi-table mode: check for ambiguity
        match self.column_index.get(&lower) {
            None => Err(TypeError::ColumnNotFound {
                name: name.to_string(),
                available: self.column_index.keys().cloned().collect(),
            }),
            Some(matches) if matches.len() > 1 => Err(TypeError::AmbiguousColumn {
                name: name.to_string(),
                tables: matches.iter().map(|(t, _)| t.to_string()).collect(),
            }),
            Some(matches) => {
                let (table_alias, col_idx) = matches[0];
                let schema = self.tables.get(table_alias).unwrap();
                Ok(ResolvedColumn {
                    table_alias: Some(table_alias),
                    column_index: col_idx,
                    column_def: &schema.columns[col_idx],
                })
            }
        }
    }

    pub fn resolve_qualified(
        &self,
        table: &'a str,
        column: &str,
    ) -> Result<ResolvedColumn<'a>, TypeError> {
        let schema = self
            .tables
            .get(table)
            .ok_or_else(|| TypeError::ColumnNotFound {
                name: format!("{}.{}", table, column),
                available: self.tables.keys().map(|s| s.to_string()).collect(),
            })?;

        for (idx, col) in schema.columns.iter().enumerate() {
            if col.name.eq_ignore_ascii_case(column) {
                return Ok(ResolvedColumn {
                    table_alias: Some(table),
                    column_index: idx,
                    column_def: col,
                });
            }
        }

        Err(TypeError::ColumnNotFound {
            name: format!("{}.{}", table, column),
            available: schema
                .columns
                .iter()
                .map(|c| format!("{}.{}", table, c.name))
                .collect(),
        })
    }

    #[allow(dead_code)]
    pub fn available_columns(&self) -> Vec<String> {
        let mut cols = Vec::new();
        for (alias, schema) in &self.tables {
            for col in &schema.columns {
                if alias.is_empty() {
                    cols.push(col.name.clone());
                } else {
                    cols.push(format!("{}.{}", alias, col.name));
                }
            }
        }
        cols
    }
}
