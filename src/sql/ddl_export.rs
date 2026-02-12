use crate::storage::TikvStore;
use crate::types::{
    ForeignKeyAction, IndexDef, SequenceDef, TableSchema, TriggerDef, UserTypeDef, UserTypeKind,
    ViewDef,
};
use anyhow::Result;
use tikv_client::Transaction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlExportRow {
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

fn column_type_sql(schema_col: &crate::types::ColumnDef) -> String {
    if schema_col.is_serial {
        return match schema_col.data_type {
            crate::types::DataType::Int64 => "BIGSERIAL".to_string(),
            _ => "SERIAL".to_string(),
        };
    }
    schema_col.data_type.to_string()
}

pub fn table_to_ddl(schema: &TableSchema) -> String {
    let mut definitions: Vec<String> = Vec::new();

    for col in &schema.columns {
        let mut col_sql = format!("{} {}", col.name, column_type_sql(col));
        if !col.nullable {
            col_sql.push_str(" NOT NULL");
        }
        if let Some(default_expr) = &col.default_expr {
            col_sql.push_str(" DEFAULT ");
            col_sql.push_str(default_expr);
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

pub async fn export_all_ddl(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
) -> Result<Vec<DdlExportRow>> {
    let mut rows: Vec<DdlExportRow> = Vec::new();

    let mut types = store.list_types(txn, db_id).await?;
    types.sort_by(|a, b| (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str())));
    for def in types {
        let object_name = format!("{}.{}", def.schema, def.name);
        rows.push(DdlExportRow {
            object_type: "type".to_string(),
            object_name,
            ddl_sql: type_to_ddl(&def),
        });
    }

    let mut sequences = store.list_sequences(txn, db_id).await?;
    sequences.sort_by_key(|d| d.full_name());
    for def in sequences {
        rows.push(DdlExportRow {
            object_type: "sequence".to_string(),
            object_name: def.full_name(),
            ddl_sql: sequence_to_ddl(&def),
        });
    }

    let mut table_names = store.list_tables(txn, db_id).await?;
    table_names.sort();
    for table_name in table_names {
        let Some(schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };
        rows.push(DdlExportRow {
            object_type: "table".to_string(),
            object_name: schema.name.clone(),
            ddl_sql: table_to_ddl(&schema),
        });
        for idx in &schema.indexes {
            rows.push(DdlExportRow {
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
            object_type: "view".to_string(),
            object_name: view.full_name(),
            ddl_sql: view_to_ddl(&view),
        });
    }

    let mut matviews = store.list_materialized_views(txn, db_id).await?;
    matviews.sort();
    for name in matviews {
        if let Some(query) = store.get_materialized_view(txn, db_id, &name).await? {
            rows.push(DdlExportRow {
                object_type: "materialized_view".to_string(),
                object_name: name.clone(),
                ddl_sql: matview_to_ddl(&name, &query),
            });
        }
    }

    let mut triggers = store.list_triggers(txn, db_id).await?;
    triggers.sort_by(|a, b| (a.table.as_str(), a.name.as_str()).cmp(&(b.table.as_str(), b.name.as_str())));
    for trigger in triggers {
        rows.push(DdlExportRow {
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
                object_type: "procedure".to_string(),
                object_name: proc_name.clone(),
                ddl_sql: procedure_to_ddl(&proc_name, &definition),
            });
        }
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CheckConstraint, ColumnDef, DataType, ForeignKeyConstraint, IndexDef, SequenceBacking,
        SequenceState,
    };

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
                },
                ColumnDef {
                    name: "email".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: true,
                    is_serial: false,
                    default_expr: Some("'x@example.com'".to_string()),
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

        let ddl = table_to_ddl(&schema);
        assert!(ddl.contains("CREATE TABLE public.users"));
        assert!(ddl.contains("id SERIAL NOT NULL"));
        assert!(ddl.contains("email TEXT NOT NULL DEFAULT 'x@example.com' UNIQUE"));
        assert!(ddl.contains("CONSTRAINT users_pkey PRIMARY KEY (id)"));
        assert!(ddl.contains("CONSTRAINT users_email_chk CHECK (email <> '')"));
        assert!(ddl.contains("CONSTRAINT users_org_fk FOREIGN KEY (id) REFERENCES public.orgs (id)"));
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
        let ddl = procedure_to_ddl(
            "public.p",
            "PARAMS:a int, b text\nBODY:SELECT a;SELECT b;",
        );
        assert!(ddl.starts_with("CREATE OR REPLACE PROCEDURE public.p(a int, b text)"));
        assert!(ddl.contains("SELECT a;SELECT b;"));
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
            method: Some("btree".to_string()),
            predicate: Some("email IS NOT NULL".to_string()),
            expressions: vec![],
        };
        let ddl = index_to_ddl("public.users", &idx);
        assert_eq!(
            ddl,
            "CREATE UNIQUE INDEX idx_users_email ON public.users USING btree (email) WHERE email IS NOT NULL;"
        );
    }
}
