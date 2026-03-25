//! Data types for the SQL engine

pub mod date;
pub mod timestamp;

mod catalog;
mod data_type;
mod metadata;
mod schema;
mod value;

// Re-export all public types for backward compatibility.
// `use crate::model::DataType` etc. continues to work.
pub use catalog::*;
pub use data_type::*;
pub use metadata::*;
pub use schema::*;
pub use value::*;

#[allow(clippy::items_after_test_module)]
pub(crate) fn default_owner() -> String {
    "postgres".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_as_bytea() {
        let v = Value::Bytes(vec![1, 2, 3]);
        assert_eq!(v.as_bytea().unwrap(), &[1, 2, 3]);
        assert!(Value::Int32(1).as_bytea().is_err());
    }

    #[test]
    fn value_as_uuid() {
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let v = Value::Uuid(*uuid.as_bytes());
        assert_eq!(v.as_uuid().unwrap(), uuid);
        assert!(Value::Text("x".into()).as_uuid().is_err());
    }

    #[test]
    fn format_vector_pg_text_integers() {
        assert_eq!(format_vector_pg_text(&[1.0, 2.0, 3.0]), "[1,2,3]");
    }

    #[test]
    fn format_vector_pg_text_mixed() {
        assert_eq!(format_vector_pg_text(&[1.0, 2.5, 3.0]), "[1,2.5,3]");
    }

    #[test]
    fn format_vector_pg_text_empty() {
        assert_eq!(format_vector_pg_text(&[]), "[]");
    }

    #[test]
    fn database_def_defaults() {
        let db = DatabaseDef::default_postgres(1, "admin".to_string());
        assert_eq!(db.id, 1);
        assert_eq!(db.name, "postgres");
        assert_eq!(db.owner, "admin");
        assert_eq!(db.encoding, "UTF8");
        assert!(db.created_at >= 0);
        assert!(db.allow_conn);
        assert!(!db.is_template);
    }

    #[test]
    fn pg_display_name_preserves_character_varying_canonical_name() {
        assert_eq!(DataType::Varchar(0).pg_display_name(), "character varying");
        assert_eq!(DataType::Varchar(42).pg_display_name(), "character varying");
        assert_eq!(
            DataType::Array(Box::new(DataType::Varchar(3))).pg_display_name(),
            "character varying[]"
        );
    }

    // ── ColumnDef::new() + modifiers ──────────────────────────────────

    #[test]
    fn columndef_new_defaults() {
        let c = ColumnDef::new("id", DataType::Int64, true);
        assert_eq!(c.name, "id");
        assert_eq!(c.data_type, DataType::Int64);
        assert!(c.nullable);
        assert!(!c.primary_key);
        assert!(!c.unique);
        assert!(!c.is_serial);
        assert!(c.default_expr.is_none());
        assert!(c.generation_expr.is_none());
        assert!(c.generation_expr_authorized_by.is_none());
        assert!(c.collation.is_none());
        assert!(!c.is_dropped);
    }

    #[test]
    fn columndef_new_not_null() {
        let c = ColumnDef::new("x", DataType::Int32, false);
        assert!(!c.nullable);
    }

    #[test]
    fn columndef_primary_key_implies_not_null() {
        let c = ColumnDef::new("id", DataType::Int64, true).primary_key();
        assert!(c.primary_key);
        assert!(!c.nullable); // PK overrides nullable
    }

    #[test]
    fn columndef_serial_implies_not_null_and_clears_default() {
        let c = ColumnDef::new("id", DataType::Int64, true)
            .default_expr("42")
            .serial();
        assert!(c.is_serial);
        assert!(!c.nullable);
        assert!(c.default_expr.is_none()); // serial clears default
    }

    #[test]
    fn columndef_unique_does_not_change_nullable() {
        let c = ColumnDef::new("email", DataType::Text, true).unique();
        assert!(c.unique);
        assert!(c.nullable); // UNIQUE allows NULLs in PG
    }

    #[test]
    fn columndef_flag_modifiers_are_order_independent() {
        let a = ColumnDef::new("id", DataType::Int64, true)
            .serial()
            .primary_key()
            .unique();
        let b = ColumnDef::new("id", DataType::Int64, true)
            .unique()
            .primary_key()
            .serial();
        assert_eq!(a.primary_key, b.primary_key);
        assert_eq!(a.is_serial, b.is_serial);
        assert_eq!(a.unique, b.unique);
        assert_eq!(a.nullable, b.nullable);
        assert_eq!(a.default_expr, b.default_expr);
    }

    #[test]
    fn columndef_serial_clears_default_expr() {
        // .serial() clears default_expr — order matters when combining
        // with .default_expr(). Always call .serial() BEFORE .default_expr()
        // if both are needed.
        let cleared = ColumnDef::new("id", DataType::Int64, true)
            .default_expr("42")
            .serial();
        assert!(
            cleared.default_expr.is_none(),
            "serial() must clear prior default_expr"
        );

        let preserved = ColumnDef::new("id", DataType::Int64, true)
            .serial()
            .default_expr("42");
        assert_eq!(preserved.default_expr.as_deref(), Some("42"));
    }

    #[test]
    fn columndef_optional_setters() {
        let c = ColumnDef::new("bio", DataType::Text, true)
            .default_expr("''")
            .collation("en_US")
            .generation_expr("lower(name)")
            .generation_expr_authorized_by("admin");
        assert_eq!(c.default_expr.as_deref(), Some("''"));
        assert_eq!(c.collation.as_deref(), Some("en_US"));
        assert_eq!(c.generation_expr.as_deref(), Some("lower(name)"));
        assert_eq!(c.generation_expr_authorized_by.as_deref(), Some("admin"));
    }

    #[test]
    fn columndef_equivalence_with_struct_literal() {
        let via_new = ColumnDef::new("_rowid", DataType::Int64, false)
            .primary_key()
            .unique()
            .serial();
        let via_literal = ColumnDef {
            name: "_rowid".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            unique: true,
            is_serial: true,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
            is_dropped: false,
        };
        assert_eq!(via_new.name, via_literal.name);
        assert_eq!(via_new.data_type, via_literal.data_type);
        assert_eq!(via_new.nullable, via_literal.nullable);
        assert_eq!(via_new.primary_key, via_literal.primary_key);
        assert_eq!(via_new.unique, via_literal.unique);
        assert_eq!(via_new.is_serial, via_literal.is_serial);
        assert_eq!(via_new.default_expr, via_literal.default_expr);
        assert_eq!(via_new.generation_expr, via_literal.generation_expr);
        assert_eq!(
            via_new.generation_expr_authorized_by,
            via_literal.generation_expr_authorized_by
        );
        assert_eq!(via_new.collation, via_literal.collation);
        assert_eq!(via_new.is_dropped, via_literal.is_dropped);
    }

    // ── TableSchema::virtual_table() ─────────────────────────────────

    #[test]
    fn virtual_table_defaults() {
        let s = TableSchema::virtual_table(
            "pg_class",
            vec![ColumnDef::new("relname", DataType::Text, true)],
        );
        assert_eq!(s.name, "pg_class");
        assert_eq!(s.table_id, 0);
        assert_eq!(s.version, 1);
        assert_eq!(s.columns.len(), 1);
        assert!(s.pk_constraint_name.is_none());
        assert!(s.pk_indices.is_empty());
        assert!(s.indexes.is_empty());
        assert!(s.check_constraints.is_empty());
        assert!(s.foreign_keys.is_empty());
        assert_eq!(s.owner, ""); // virtual tables use empty owner
        assert!(!s.rls_enabled);
        assert!(!s.rls_force);
        assert!(s.from_alias.is_none());
    }

    #[test]
    fn virtual_table_equivalence_with_struct_literal() {
        let cols = vec![ColumnDef::new("oid", DataType::Int64, false)];
        let via_factory = TableSchema::virtual_table("pg_type", cols.clone());
        let via_literal = TableSchema {
            table_id: 0,
            name: "pg_type".to_string(),
            columns: cols,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        };
        assert_eq!(via_factory.name, via_literal.name);
        assert_eq!(via_factory.table_id, via_literal.table_id);
        assert_eq!(via_factory.version, via_literal.version);
        assert_eq!(via_factory.owner, via_literal.owner);
        assert_eq!(via_factory.columns.len(), via_literal.columns.len());
    }
}
