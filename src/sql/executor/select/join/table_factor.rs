use super::super::*;

static NESTED_JOIN_ALIAS_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_nested_join_alias() -> String {
    let id = NESTED_JOIN_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__tipg_nested_join_{}", id)
}

#[derive(Debug, Clone)]
pub(super) struct TransparentNestedJoinInfo {
    pub(super) derived_alias: String,
    pub(super) inner_aliases: Vec<String>,
    pub(super) duplicate_cols_lower: HashSet<String>,
}

pub(super) fn collect_visible_aliases_in_table_with_joins(
    table_with_joins: &sqlparser::ast::TableWithJoins,
) -> Vec<String> {
    fn exposed_name_for_object(name: &ObjectName) -> String {
        names::split_object_name(name)
            .map(|(_, obj)| obj)
            .unwrap_or_else(|_| {
                name.0
                    .last()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_default()
            })
    }

    fn collect_from_factor(factor: &TableFactor, out: &mut Vec<String>) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let exposed = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| exposed_name_for_object(name));
                if !exposed.is_empty() {
                    out.push(exposed);
                }
            }
            TableFactor::Derived { alias, .. } => {
                if let Some(a) = alias.as_ref() {
                    out.push(a.name.value.clone());
                }
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                if let Some(a) = alias.as_ref() {
                    // An aliased NestedJoin is a derived table; inner aliases must not leak.
                    out.push(a.name.value.clone());
                } else {
                    collect_from_table_with_joins(table_with_joins, out);
                }
            }
            _ => {}
        }
    }

    fn collect_from_table_with_joins(twj: &sqlparser::ast::TableWithJoins, out: &mut Vec<String>) {
        collect_from_factor(&twj.relation, out);
        for join in &twj.joins {
            collect_from_factor(&join.relation, out);
        }
    }

    let mut aliases = Vec::new();
    collect_from_table_with_joins(table_with_joins, &mut aliases);

    let mut seen: HashSet<String> = HashSet::new();
    aliases.retain(|a| seen.insert(a.to_lowercase()));
    aliases
}

pub(super) fn duplicate_column_names_lowercase(schema: &TableSchema) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for col in &schema.columns {
        *counts.entry(col.name.to_lowercase()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

pub(super) fn extract_virtual_table_filter(expr: &Expr) -> VirtualTableFilter {
    fn column_ref_name(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Identifier(ident) => Some(ident.value.as_str()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.as_str()),
            _ => None,
        }
    }

    fn string_literal(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Value(SqlValue::SingleQuotedString(s))
            | Expr::Value(SqlValue::DoubleQuotedString(s))
            | Expr::Value(SqlValue::NationalStringLiteral(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn merge_string_slot(slot: &mut Option<String>, val: &str) {
        match slot {
            None => *slot = Some(val.to_string()),
            Some(existing) => {
                if !existing.eq_ignore_ascii_case(val) {
                    *slot = None;
                }
            }
        }
    }

    fn visit(expr: &Expr, out: &mut VirtualTableFilter, saw_or: &mut bool) {
        match expr {
            Expr::BinaryOp { left, op, right } => {
                if matches!(op, BinaryOperator::Or) {
                    *saw_or = true;
                    return;
                }

                if matches!(op, BinaryOperator::And) {
                    visit(left, out, saw_or);
                    visit(right, out, saw_or);
                    return;
                }

                if matches!(op, BinaryOperator::Eq) {
                    let (col, lit) = if let (Some(col), Some(lit)) =
                        (column_ref_name(left), string_literal(right))
                    {
                        (col, lit)
                    } else if let (Some(col), Some(lit)) =
                        (column_ref_name(right), string_literal(left))
                    {
                        (col, lit)
                    } else {
                        return;
                    };

                    if col.eq_ignore_ascii_case("table_name") || col.eq_ignore_ascii_case("relname")
                    {
                        merge_string_slot(&mut out.table_name, lit);
                    } else if col.eq_ignore_ascii_case("table_schema") {
                        merge_string_slot(&mut out.table_schema, lit);
                    }
                }
            }
            Expr::Nested(inner)
            | Expr::UnaryOp { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::TryCast { expr: inner, .. } => {
                visit(inner, out, saw_or);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                visit(expr, out, saw_or);
                visit(low, out, saw_or);
                visit(high, out, saw_or);
            }
            Expr::InList { expr, list, .. } => {
                visit(expr, out, saw_or);
                for item in list {
                    visit(item, out, saw_or);
                }
            }
            _ => {}
        }
    }

    let mut out = VirtualTableFilter::default();
    let mut saw_or = false;
    visit(expr, &mut out, &mut saw_or);
    if saw_or {
        VirtualTableFilter::default()
    } else {
        out
    }
}

impl Executor {
    pub(super) async fn resolve_join_table_factor(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_factor: &TableFactor,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        virtual_filter: &VirtualTableFilter,
    ) -> Result<Option<(String, TableSchema, Option<Vec<Row>>)>> {
        match table_factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let (schema_opt, obj_name) = names::split_object_name(name)?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| obj_name.clone());

                if let Some(func_args) = args {
                    let tbl_upper = obj_name.to_uppercase();
                    if tbl_upper == "GENERATE_SERIES" {
                        let (schema, rows) = self
                            .execute_generate_series(func_args, &alias_str, alias.as_ref(), 0, None)
                            .await?;
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                    if let Some((schema, rows)) = self
                        .try_execute_extension_table_function(
                            txn,
                            db_id,
                            search_path,
                            name,
                            func_args,
                            alias.as_ref(),
                        )
                        .await?
                    {
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                    if let Some((schema, rows)) = self
                        .try_execute_user_table_function(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            name,
                            func_args,
                            alias.as_ref(),
                        )
                        .await?
                    {
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                }

                let cte_key = obj_name.to_lowercase();
                if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                    return Ok(Some((
                        alias_str,
                        cte_schema.clone(),
                        Some(cte_rows.clone()),
                    )));
                }
                if let Some(resolved) = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    name,
                    search_path,
                )
                .await?
                {
                    if let Some(s) = self.store().get_schema(txn, db_id, &resolved.full).await? {
                        return Ok(Some((alias_str, s, None)));
                    }
                }
                let lookup_name = match schema_opt {
                    Some(schema) => format!("{}.{}", schema, obj_name),
                    None => obj_name.clone(),
                };
                match self
                    .get_table_data_filtered(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &lookup_name,
                        ctes,
                        virtual_filter,
                    )
                    .await
                {
                    Ok((schema, rows)) => Ok(Some((alias_str, schema, Some(rows)))),
                    Err(_) => Ok(None),
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        subquery,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;
                Ok(Some((alias_name, schema, Some(rows))))
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                // KISS fallback: materialize the nested join as a derived table.
                // This avoids hard errors for `TableFactor::NestedJoin` without having to teach the
                // operator join planner about every nested join shape up-front.
                let alias_name = if let Some(a) = alias.as_ref() {
                    a.name.value.clone()
                } else {
                    next_nested_join_alias()
                };
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);

                debug!(
                    alias = %alias_name,
                    nested_join = %table_with_joins,
                    "Materializing nested join as derived table"
                );

                let nested_query = Query {
                    with: None,
                    body: Box::new(SetExpr::Select(Box::new(sqlparser::ast::Select {
                        distinct: None,
                        top: None,
                        projection: vec![SelectItem::Wildcard(Default::default())],
                        into: None,
                        from: vec![(*table_with_joins.as_ref()).clone()],
                        lateral_views: vec![],
                        selection: None,
                        group_by: GroupByExpr::Expressions(vec![]),
                        cluster_by: vec![],
                        distribute_by: vec![],
                        sort_by: vec![],
                        having: None,
                        named_window: vec![],
                        qualify: None,
                    }))),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    limit_by: vec![],
                    for_clause: None,
                };

                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &nested_query,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;

                let derived_cols: Vec<String> =
                    schema.columns.iter().map(|c| c.name.clone()).collect();
                debug!(
                    alias = %alias_name,
                    derived_cols = ?derived_cols,
                    "Nested join derived table output schema"
                );
                Ok(Some((alias_name, schema, Some(rows))))
            }
            _ => Ok(None),
        }
    }

}
