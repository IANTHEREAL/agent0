use crate::model::{
    ForeignKeyAction, IndexDef, SequenceDef, TableSchema, TriggerDef, UserTypeDef, UserTypeKind,
    ViewDef,
};
use crate::storage::TikvStore;
use anyhow::Result;
use std::collections::HashMap;
use tikv_client::Transaction;

use super::sequences;
use super::sequences::SerialDefaultBehavior;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlExportRow {
    pub ddl_order: i64,
    pub object_type: String,
    pub object_name: String,
    pub ddl_sql: String,
}

fn quote_sql_string(input: &str) -> String {
    format!("'{}'", input.replace('\'', "''"))
}

fn foreign_key_action_sql(action: &ForeignKeyAction) -> &'static str {
    match action {
        ForeignKeyAction::NoAction => "NO ACTION",
        ForeignKeyAction::Restrict => "RESTRICT",
        ForeignKeyAction::Cascade => "CASCADE",
        ForeignKeyAction::SetNull => "SET NULL",
        ForeignKeyAction::SetDefault => "SET DEFAULT",
    }
}

fn column_type_sql(schema_col: &crate::model::ColumnDef) -> String {
    schema_col.data_type.to_string()
}

pub fn table_to_ddl(schema: &TableSchema, serial_sequences: &HashMap<String, String>) -> String {
    let mut definitions: Vec<String> = Vec::new();

    for col in &schema.columns {
        let mut col_sql = format!("{} {}", col.name, column_type_sql(col));
        if !col.nullable {
            col_sql.push_str(" NOT NULL");
        }
        let default_expr = if col.is_serial {
            match sequences::classify_serial_default(col.default_expr.as_deref()) {
                SerialDefaultBehavior::ExplicitExpr(expr) => Some(expr.to_string()),
                SerialDefaultBehavior::ExplicitNull => None,
                SerialDefaultBehavior::ImplicitSequence => serial_sequences
                    .get(&col.name)
                    .map(|seq_full_name| format!("nextval('{}'::regclass)", seq_full_name)),
            }
        } else {
            col.default_expr.clone()
        };
        if let Some(default_expr) = default_expr {
            col_sql.push_str(" DEFAULT ");
            col_sql.push_str(&default_expr);
        }
        if col.unique {
            col_sql.push_str(" UNIQUE");
        }
        definitions.push(col_sql);
    }

    if !schema.pk_indices.is_empty() {
        let pk_columns: Vec<String> = schema
            .pk_indices
            .iter()
            .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
            .collect();
        let pk_body = format!("PRIMARY KEY ({})", pk_columns.join(", "));
        if let Some(name) = &schema.pk_constraint_name {
            definitions.push(format!("CONSTRAINT {} {}", name, pk_body));
        } else {
            definitions.push(pk_body);
        }
    }

    for check in &schema.check_constraints {
        if let Some(name) = &check.name {
            definitions.push(format!("CONSTRAINT {} CHECK ({})", name, check.expr));
        } else {
            definitions.push(format!("CHECK ({})", check.expr));
        }
    }

    for fk in &schema.foreign_keys {
        definitions.push(format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
            fk.name,
            fk.columns.join(", "),
            fk.ref_table,
            fk.ref_columns.join(", "),
            foreign_key_action_sql(&fk.on_delete),
            foreign_key_action_sql(&fk.on_update)
        ));
    }

    format!(
        "CREATE TABLE {} (\n    {}\n);",
        schema.name,
        definitions.join(",\n    ")
    )
}

fn index_to_ddl(table_name: &str, idx: &IndexDef) -> String {
    let method = idx.method.as_deref().unwrap_or("btree");
    let mut parts = idx.columns.clone();
    parts.extend(idx.expressions.iter().map(|e| format!("({})", e)));
    let mut ddl = format!(
        "CREATE {}INDEX {} ON {} USING {} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        idx.name,
        table_name,
        method,
        parts.join(", ")
    );
    if let Some(predicate) = &idx.predicate {
        ddl.push_str(" WHERE ");
        ddl.push_str(predicate);
    }
    ddl.push(';');
    ddl
}

pub fn view_to_ddl(view: &ViewDef) -> String {
    format!("CREATE VIEW {} AS {};", view.full_name(), view.query)
}

pub fn matview_to_ddl(name: &str, query: &str) -> String {
    format!("CREATE MATERIALIZED VIEW {} AS {};", name, query)
}

pub fn sequence_to_ddl(def: &SequenceDef) -> String {
    format!(
        "CREATE SEQUENCE {} INCREMENT {} MINVALUE {} MAXVALUE {} START {} {};",
        def.full_name(),
        def.increment,
        def.min_value,
        def.max_value,
        def.start_value,
        if def.is_cycled { "CYCLE" } else { "NO CYCLE" }
    )
}

pub fn sequence_owned_by_to_ddl(
    sequence_name: &str,
    table_name: &str,
    column_name: &str,
) -> String {
    format!(
        "ALTER SEQUENCE {} OWNED BY {}.{};",
        sequence_name, table_name, column_name
    )
}

pub fn type_to_ddl(def: &UserTypeDef) -> String {
    let full_name = format!("{}.{}", def.schema, def.name);
    match &def.kind {
        UserTypeKind::Enum { labels } => {
            let values = labels
                .iter()
                .map(|label| quote_sql_string(label))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TYPE {} AS ENUM ({});", full_name, values)
        }
        UserTypeKind::Composite { fields } => {
            let fields_sql = fields
                .iter()
                .map(|(name, data_type)| format!("{} {}", name, data_type))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TYPE {} AS ({});", full_name, fields_sql)
        }
    }
}

pub fn procedure_to_ddl(name: &str, definition: &str) -> String {
    let (params, body) = if let Some((left, right)) = definition.split_once("\nBODY:") {
        (
            left.trim_start_matches("PARAMS:").trim().to_string(),
            right.trim().to_string(),
        )
    } else {
        (String::new(), definition.trim().to_string())
    };

    format!(
        "CREATE OR REPLACE PROCEDURE {}({}) AS BEGIN\n{}\nEND;",
        name, params, body
    )
}

pub fn function_to_ddl(def: &crate::model::FunctionDef) -> String {
    let full_name = format!("{}.{}", def.schema, def.name);
    let args = def.arg_types.join(", ");
    let return_type = def.return_type.trim();
    let language = def.language.trim();
    let body = def.body.trim();

    format!(
        "CREATE OR REPLACE FUNCTION {}({}) RETURNS {} AS $$\n{}\n$$ LANGUAGE {};",
        full_name, args, return_type, body, language
    )
}

pub fn trigger_to_ddl(def: &TriggerDef) -> String {
    let events = if def.events.is_empty() {
        "INSERT".to_string()
    } else {
        def.events.join(" OR ")
    };
    format!(
        "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW EXECUTE FUNCTION {}();",
        def.name, def.timing, events, def.table, def.function
    )
}

/// Topological sort of table names by FK dependencies.
/// Tables referenced by foreign keys come before the tables that reference them.
/// Falls back to alphabetical order for tables without dependencies or cycles.
fn toposort_tables_by_fk(
    table_names: &[String],
    schemas: &HashMap<String, TableSchema>,
) -> Vec<String> {
    use std::collections::HashSet;

    let name_set: HashSet<&str> = table_names.iter().map(|s| s.as_str()).collect();

    // Build adjacency: table -> set of tables it depends on (FK references)
    // Use HashSet to deduplicate (a table with multiple FKs to the same parent
    // should count as a single dependency for in-degree computation).
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for name in table_names {
        let mut table_deps = HashSet::new();
        if let Some(schema) = schemas.get(name) {
            for fk in &schema.foreign_keys {
                // Only add dependency if referenced table is in our set and is not self-referential
                if name_set.contains(fk.ref_table.as_str()) && fk.ref_table != *name {
                    table_deps.insert(fk.ref_table.as_str());
                }
            }
        }
        deps.insert(name.as_str(), table_deps);
    }

    // Kahn's algorithm for topological sort
    // in_degree[name] = number of FK dependencies name has (must be created after those)
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for name in table_names {
        in_degree.insert(
            name.as_str(),
            deps.get(name.as_str()).map_or(0, |d| d.len()),
        );
    }

    let mut queue: Vec<&str> = table_names
        .iter()
        .filter(|n| in_degree.get(n.as_str()) == Some(&0))
        .map(|n| n.as_str())
        .collect();
    queue.sort_by(|a, b| b.cmp(a)); // descending so pop() yields ascending alphabetical

    let mut result = Vec::with_capacity(table_names.len());
    while let Some(name) = queue.pop() {
        result.push(name.to_string());
        // For each table that depends on `name`, decrement its in-degree
        for other in table_names {
            if let Some(other_deps) = deps.get(other.as_str()) {
                if other_deps.contains(&name) {
                    let deg = in_degree.get_mut(other.as_str()).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        // Insert in sorted position to maintain deterministic order
                        let pos = queue.partition_point(|q| *q > other.as_str());
                        queue.insert(pos, other.as_str());
                    }
                }
            }
        }
    }

    // If there's a cycle, append remaining tables in alphabetical order
    if result.len() < table_names.len() {
        let result_set: HashSet<&str> = result.iter().map(|s| s.as_str()).collect();
        let mut remaining: Vec<String> = table_names
            .iter()
            .filter(|n| !result_set.contains(n.as_str()))
            .cloned()
            .collect();
        remaining.sort();
        result.extend(remaining);
    }

    result
}

pub async fn export_all_ddl(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
) -> Result<Vec<DdlExportRow>> {
    let mut rows: Vec<DdlExportRow> = Vec::new();

    let mut types = store.list_types(txn, db_id).await?;
    types.sort_by(|a, b| {
        (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str()))
    });
    for def in types {
        let object_name = format!("{}.{}", def.schema, def.name);
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "type".to_string(),
            object_name,
            ddl_sql: type_to_ddl(&def),
        });
    }

    let mut sequences = store.list_sequences(txn, db_id).await?;
    sequences.sort_by_key(|d| d.full_name());
    for def in &sequences {
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "sequence".to_string(),
            object_name: def.full_name(),
            ddl_sql: sequence_to_ddl(def),
        });
    }

    let mut table_names = store.list_tables(txn, db_id).await?;
    table_names.sort();

    // Topological sort: tables referenced by FKs must come before referencing tables.
    let mut schemas_map: HashMap<String, TableSchema> = HashMap::new();
    for table_name in &table_names {
        if let Some(schema) = store.get_schema(txn, db_id, table_name).await? {
            schemas_map.insert(table_name.clone(), schema);
        }
    }
    let table_names = toposort_tables_by_fk(&table_names, &schemas_map);

    for table_name in table_names {
        let Some(schema) = schemas_map.remove(&table_name) else {
            continue;
        };

        let (table_schema, table_object_name) = schema
            .name
            .rsplit_once('.')
            .unwrap_or(("public", schema.name.as_str()));
        let mut serial_sequences: HashMap<String, String> = HashMap::new();
        for col in &schema.columns {
            if !col.is_serial {
                continue;
            }
            let seq_full_name = match sequences::find_owned_sequence_full_name(
                &sequences,
                &schema.name,
                &col.name,
            )? {
                Some(name) => name,
                None => format!(
                    "{}.{}",
                    table_schema,
                    sequences::implicit_sequence_name(table_object_name, &col.name)
                ),
            };
            serial_sequences.insert(col.name.clone(), seq_full_name);
        }

        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "table".to_string(),
            object_name: schema.name.clone(),
            ddl_sql: table_to_ddl(&schema, &serial_sequences),
        });

        let mut owned_sequences = sequences
            .iter()
            .filter_map(|def| {
                let Some((owned_table, owned_col)) = &def.owned_by else {
                    return None;
                };
                if owned_table != &schema.name {
                    return None;
                }
                Some((def.full_name(), owned_col.clone()))
            })
            .collect::<Vec<_>>();
        owned_sequences.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (sequence_name, column_name) in owned_sequences {
            rows.push(DdlExportRow {
                ddl_order: 0,
                object_type: "sequence_ownership".to_string(),
                object_name: sequence_name.clone(),
                ddl_sql: sequence_owned_by_to_ddl(&sequence_name, &schema.name, &column_name),
            });
        }

        for idx in &schema.indexes {
            rows.push(DdlExportRow {
                ddl_order: 0,
                object_type: "index".to_string(),
                object_name: idx.name.clone(),
                ddl_sql: index_to_ddl(&schema.name, idx),
            });
        }
    }

    let mut views = store.list_views(txn, db_id).await?;
    views.sort_by_key(|v| v.full_name());
    for view in views {
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "view".to_string(),
            object_name: view.full_name(),
            ddl_sql: view_to_ddl(&view),
        });
    }

    let mut matviews = store.list_materialized_views(txn, db_id).await?;
    matviews.sort_by_key(|v| v.full_name());
    for matview in matviews {
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "materialized_view".to_string(),
            object_name: matview.full_name(),
            ddl_sql: matview_to_ddl(&matview.full_name(), &matview.query),
        });
    }

    let mut functions = store.list_functions(txn, db_id).await?;
    functions.sort_by(|a, b| {
        (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str()))
    });
    for function in functions {
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "function".to_string(),
            object_name: format!("{}.{}", function.schema, function.name),
            ddl_sql: function_to_ddl(&function),
        });
    }

    let mut triggers = store.list_triggers(txn, db_id).await?;
    triggers.sort_by(|a, b| {
        (a.table.as_str(), a.name.as_str()).cmp(&(b.table.as_str(), b.name.as_str()))
    });
    for trigger in triggers {
        rows.push(DdlExportRow {
            ddl_order: 0,
            object_type: "trigger".to_string(),
            object_name: format!("{}.{}", trigger.table, trigger.name),
            ddl_sql: trigger_to_ddl(&trigger),
        });
    }

    let mut procedures = store.list_procedures(txn, db_id).await?;
    procedures.sort();
    for proc_name in procedures {
        if let Some(definition) = store.get_procedure(txn, db_id, &proc_name).await? {
            rows.push(DdlExportRow {
                ddl_order: 0,
                object_type: "procedure".to_string(),
                object_name: proc_name.clone(),
                ddl_sql: procedure_to_ddl(&proc_name, &definition),
            });
        }
    }

    for (idx, row) in rows.iter_mut().enumerate() {
        row.ddl_order = i64::try_from(idx + 1).unwrap_or(i64::MAX);
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CheckConstraint, ColumnDef, DataType, ForeignKeyConstraint, IndexDef, SequenceBacking,
        SequenceState,
    };
    use std::collections::HashMap;

    #[test]
    fn table_to_ddl_includes_constraints() {
        let schema = TableSchema {
            name: "public.users".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: true,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "email".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: true,
                    is_serial: false,
                    default_expr: Some("'x@example.com'".to_string()),
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("users_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![CheckConstraint {
                name: Some("users_email_chk".to_string()),
                expr: "email <> ''".to_string(),
            }],
            foreign_keys: vec![ForeignKeyConstraint {
                name: "users_org_fk".to_string(),
                columns: vec!["id".to_string()],
                ref_table: "public.orgs".to_string(),
                ref_columns: vec!["id".to_string()],
                on_delete: ForeignKeyAction::Cascade,
                on_update: ForeignKeyAction::NoAction,
            }],
            owner: "postgres".to_string(),
            from_alias: None,
        };

        let mut serial_sequences = HashMap::new();
        serial_sequences.insert("id".to_string(), "public.users_id_seq".to_string());
        let ddl = table_to_ddl(&schema, &serial_sequences);
        assert!(ddl.contains("CREATE TABLE public.users"));
        assert!(
            ddl.contains("id INTEGER NOT NULL DEFAULT nextval('public.users_id_seq'::regclass)")
        );
        assert!(ddl.contains("email TEXT NOT NULL DEFAULT 'x@example.com' UNIQUE"));
        assert!(ddl.contains("CONSTRAINT users_pkey PRIMARY KEY (id)"));
        assert!(ddl.contains("CONSTRAINT users_email_chk CHECK (email <> '')"));
        assert!(
            ddl.contains("CONSTRAINT users_org_fk FOREIGN KEY (id) REFERENCES public.orgs (id)")
        );
    }

    #[test]
    fn table_to_ddl_serial_uses_explicit_default_expression() {
        let schema = TableSchema {
            name: "public.users".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: true,
                default_expr: Some("42".to_string()),
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        };

        let mut serial_sequences = HashMap::new();
        serial_sequences.insert("id".to_string(), "public.users_id_seq".to_string());
        let ddl = table_to_ddl(&schema, &serial_sequences);
        assert!(ddl.contains("id INTEGER NOT NULL DEFAULT 42"));
        assert!(!ddl.contains("nextval("));
    }

    #[test]
    fn table_to_ddl_serial_drop_default_omits_default_clause() {
        let schema = TableSchema {
            name: "public.users".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: true,
                default_expr: Some(crate::sql::sequences::serial_default_dropped_marker().into()),
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        };

        let mut serial_sequences = HashMap::new();
        serial_sequences.insert("id".to_string(), "public.users_id_seq".to_string());
        let ddl = table_to_ddl(&schema, &serial_sequences);
        assert!(ddl.contains("id INTEGER NOT NULL"));
        assert!(!ddl.contains("DEFAULT"));
    }

    #[test]
    fn sequence_to_ddl_uses_core_options() {
        let seq = SequenceDef {
            oid: 1,
            schema: "public".to_string(),
            name: "s".to_string(),
            start_value: 10,
            increment: 2,
            min_value: 1,
            max_value: 999,
            cache_size: 1,
            is_cycled: true,
            owned_by: None,
            owner: "postgres".to_string(),
            backing: SequenceBacking::Standalone(SequenceState {
                last_value: 10,
                is_called: false,
            }),
        };

        let ddl = sequence_to_ddl(&seq);
        assert_eq!(
            ddl,
            "CREATE SEQUENCE public.s INCREMENT 2 MINVALUE 1 MAXVALUE 999 START 10 CYCLE;"
        );
    }

    #[test]
    fn type_to_ddl_enum_quotes_values() {
        let ty = UserTypeDef {
            oid: 1,
            schema: "public".to_string(),
            name: "status".to_string(),
            kind: UserTypeKind::Enum {
                labels: vec!["new".to_string(), "o'clock".to_string()],
            },
            owner: "postgres".to_string(),
        };

        let ddl = type_to_ddl(&ty);
        assert_eq!(
            ddl,
            "CREATE TYPE public.status AS ENUM ('new', 'o''clock');"
        );
    }

    #[test]
    fn procedure_to_ddl_parses_stored_definition() {
        let ddl = procedure_to_ddl("public.p", "PARAMS:a int, b text\nBODY:SELECT a;SELECT b;");
        assert!(ddl.starts_with("CREATE OR REPLACE PROCEDURE public.p(a int, b text)"));
        assert!(ddl.contains("SELECT a;SELECT b;"));
    }

    #[test]
    fn function_to_ddl_formats_body_and_language() {
        let def = crate::model::FunctionDef {
            oid: 1,
            schema: "public".to_string(),
            name: "audit_fn".to_string(),
            arg_types: vec![],
            return_type: "trigger".to_string(),
            language: "plpgsql".to_string(),
            body: "BEGIN RETURN NEW; END;".to_string(),
            owner: "postgres".to_string(),
        };
        let ddl = function_to_ddl(&def);
        assert!(ddl.starts_with("CREATE OR REPLACE FUNCTION public.audit_fn() RETURNS trigger"));
        assert!(ddl.contains("AS $$"));
        assert!(ddl.contains("BEGIN RETURN NEW; END;"));
        assert!(ddl.ends_with("LANGUAGE plpgsql;"));
    }

    #[test]
    fn trigger_to_ddl_formats_events() {
        let def = TriggerDef {
            oid: 1,
            schema: "public".to_string(),
            name: "t_users".to_string(),
            table: "public.users".to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string(), "UPDATE".to_string()],
            function: "public.audit_fn".to_string(),
        };
        let ddl = trigger_to_ddl(&def);
        assert_eq!(
            ddl,
            "CREATE TRIGGER t_users BEFORE INSERT OR UPDATE ON public.users FOR EACH ROW EXECUTE FUNCTION public.audit_fn();"
        );
    }

    #[test]
    fn index_to_ddl_includes_method_and_predicate() {
        let idx = IndexDef {
            name: "idx_users_email".to_string(),
            id: 1,
            columns: vec!["email".to_string()],
            unique: true,
            is_constraint: false,
            method: Some("btree".to_string()),
            predicate: Some("email IS NOT NULL".to_string()),
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let ddl = index_to_ddl("public.users", &idx);
        assert_eq!(
            ddl,
            "CREATE UNIQUE INDEX idx_users_email ON public.users USING btree (email) WHERE email IS NOT NULL;"
        );
    }

    #[test]
    fn sequence_owned_by_to_ddl_formats_statement() {
        let ddl = sequence_owned_by_to_ddl("public.users_id_seq", "public.users", "id");
        assert_eq!(
            ddl,
            "ALTER SEQUENCE public.users_id_seq OWNED BY public.users.id;"
        );
    }

    fn make_schema(name: &str, fk_refs: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: fk_refs
                .iter()
                .map(|rt| ForeignKeyConstraint {
                    name: format!("fk_{}", rt),
                    columns: vec!["id".to_string()],
                    ref_table: rt.to_string(),
                    ref_columns: vec!["id".to_string()],
                    on_delete: ForeignKeyAction::NoAction,
                    on_update: ForeignKeyAction::NoAction,
                })
                .collect(),
            owner: "postgres".to_string(),
            from_alias: None,
        }
    }

    #[test]
    fn toposort_no_deps_returns_alphabetical() {
        let names = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let mut schemas = HashMap::new();
        for n in &names {
            schemas.insert(n.clone(), make_schema(n, &[]));
        }
        let result = toposort_tables_by_fk(&names, &schemas);
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn toposort_child_after_parent() {
        // child references parent; alphabetically child < parent
        let names = vec!["public.child".to_string(), "public.parent".to_string()];
        let mut schemas = HashMap::new();
        schemas.insert(
            "public.parent".to_string(),
            make_schema("public.parent", &[]),
        );
        schemas.insert(
            "public.child".to_string(),
            make_schema("public.child", &["public.parent"]),
        );
        let result = toposort_tables_by_fk(&names, &schemas);
        // parent must come before child
        let parent_pos = result.iter().position(|s| s == "public.parent").unwrap();
        let child_pos = result.iter().position(|s| s == "public.child").unwrap();
        assert!(
            parent_pos < child_pos,
            "parent ({}) should come before child ({}), got {:?}",
            parent_pos,
            child_pos,
            result
        );
    }

    #[test]
    fn toposort_chain_a_refs_b_refs_c() {
        // a -> b -> c  (a depends on b, b depends on c)
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), make_schema("a", &["b"]));
        schemas.insert("b".to_string(), make_schema("b", &["c"]));
        schemas.insert("c".to_string(), make_schema("c", &[]));
        let result = toposort_tables_by_fk(&names, &schemas);
        let pos_a = result.iter().position(|s| s == "a").unwrap();
        let pos_b = result.iter().position(|s| s == "b").unwrap();
        let pos_c = result.iter().position(|s| s == "c").unwrap();
        assert!(pos_c < pos_b, "c before b: {:?}", result);
        assert!(pos_b < pos_a, "b before a: {:?}", result);
    }

    #[test]
    fn toposort_cycle_includes_all_tables() {
        // a -> b -> a (cycle)
        let names = vec!["a".to_string(), "b".to_string()];
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), make_schema("a", &["b"]));
        schemas.insert("b".to_string(), make_schema("b", &["a"]));
        let result = toposort_tables_by_fk(&names, &schemas);
        assert_eq!(result.len(), 2, "all tables present: {:?}", result);
        assert!(result.contains(&"a".to_string()));
        assert!(result.contains(&"b".to_string()));
    }

    #[test]
    fn toposort_multi_fk_to_same_parent_with_downstream() {
        // A has 2 FKs to B (e.g. author_id and reviewer_id both reference B).
        // C has 1 FK to A.  Expected order: B, A, C.
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut schemas = HashMap::new();
        schemas.insert(
            "a".to_string(),
            make_schema("a", &["b", "b"]), // two FKs to same parent
        );
        schemas.insert("b".to_string(), make_schema("b", &[]));
        schemas.insert("c".to_string(), make_schema("c", &["a"]));
        let result = toposort_tables_by_fk(&names, &schemas);
        assert_eq!(result, vec!["b", "a", "c"]);
    }

    #[test]
    fn toposort_self_referential_ignored() {
        // a references itself — should not create a dependency
        let names = vec!["a".to_string()];
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), make_schema("a", &["a"]));
        let result = toposort_tables_by_fk(&names, &schemas);
        assert_eq!(result, vec!["a"]);
    }
}
