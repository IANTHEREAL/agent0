//! Unit tests for sequence ownership lookup.

#[cfg(test)]
mod owned_sequence_lookup_tests {
    use crate::model::{SequenceBacking, SequenceDef, SequenceState};
    use crate::sql::sequences::find_owned_sequence_full_name;

    fn make_sequence(full_name: &str, owned_by: Option<(&str, &str)>) -> SequenceDef {
        let (schema, name) = full_name.split_once('.').unwrap_or(("public", full_name));
        SequenceDef {
            oid: 0,
            schema: schema.to_string(),
            name: name.to_string(),
            start_value: 1,
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            cache_size: 1,
            is_cycled: false,
            owned_by: owned_by.map(|(table, col)| (table.to_string(), col.to_string())),
            owner: "postgres".to_string(),
            backing: SequenceBacking::Standalone(SequenceState {
                last_value: 1,
                is_called: false,
            }),
        }
    }

    #[test]
    fn finds_owned_sequence_after_table_rename() {
        let sequences = vec![make_sequence("public.t_a_seq", Some(("public.t2", "a")))];
        let found = find_owned_sequence_full_name(&sequences, "public.t2", "a").unwrap();
        assert_eq!(found.as_deref(), Some("public.t_a_seq"));
    }

    #[test]
    fn finds_owned_sequence_after_column_rename() {
        let sequences = vec![make_sequence("public.t_a_seq", Some(("public.t2", "b")))];
        let found = find_owned_sequence_full_name(&sequences, "public.t2", "b").unwrap();
        assert_eq!(found.as_deref(), Some("public.t_a_seq"));
    }

    #[test]
    fn returns_none_when_no_owned_sequence_matches() {
        let sequences = vec![make_sequence("public.t_a_seq", Some(("public.t", "a")))];
        let found = find_owned_sequence_full_name(&sequences, "public.t2", "a").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn errors_when_multiple_owned_sequences_match() {
        let sequences = vec![
            make_sequence("public.s1", Some(("public.t", "a"))),
            make_sequence("public.s2", Some(("public.t", "a"))),
        ];
        let err = find_owned_sequence_full_name(&sequences, "public.t", "a").unwrap_err();
        assert!(err.to_string().contains("Multiple sequences"));
    }

    #[test]
    fn implicit_sequence_helpers_and_name_normalization() {
        use crate::model::DataType;
        use crate::sql::sequences::{
            build_implicit_sequence_def, format_serial_sequence_name, implicit_sequence_name,
            implicit_sequence_name_with_suffix, normalize_sequence_name,
        };
        use sqlparser::ast::{Ident, ObjectName};

        assert_eq!(implicit_sequence_name("t", "id"), "t_id_seq");
        assert_eq!(
            implicit_sequence_name_with_suffix("t", "id", 1),
            "t_id_seq1"
        );

        let def = build_implicit_sequence_def("public.t", "id", &DataType::Int32);
        assert_eq!(def.schema, "public");
        assert_eq!(def.name, "t_id_seq");
        assert_eq!(def.max_value, i32::MAX as i64);

        let long_table = "tbl_name_with_many_bytes_abcdefghijklmnopqrstuvwxyz_1234567890";
        let long_col = "column_name_with_many_bytes_abcdefghijklmnopqrstuvwxyz_1234567890";
        let long_name = implicit_sequence_name(long_table, long_col);
        assert_eq!(
            long_name,
            "tbl_name_with_many_bytes_abcd_column_name_with_many_bytes_a_seq"
        );
        assert_eq!(long_name.len(), 63);

        let long_name_with_suffix = implicit_sequence_name_with_suffix(long_table, long_col, 42);
        assert_eq!(
            long_name_with_suffix,
            "tbl_name_with_many_bytes_abc_column_name_with_many_bytes__seq42"
        );
        assert_eq!(long_name_with_suffix.len(), 63);

        let utf8_name = implicit_sequence_name("名字名字名字名字名字名字", "列列列列列列列列");
        assert!(utf8_name.len() <= 63);
        assert!(utf8_name.ends_with("_seq"));

        assert_eq!(
            format_serial_sequence_name("public", "mixed_case_seq"),
            "public.mixed_case_seq"
        );
        assert_eq!(
            format_serial_sequence_name("select", "mixed_case_seq"),
            "\"select\".mixed_case_seq"
        );
        assert_eq!(
            format_serial_sequence_name("public", "mixed_Case_seq"),
            "public.\"mixed_Case_seq\""
        );

        let name = ObjectName(vec![Ident::new("my_seq")]);
        let (schema, seq, full) =
            normalize_sequence_name(&name, &["app".to_string(), "public".to_string()]).unwrap();
        assert_eq!(schema, "app");
        assert_eq!(seq, "my_seq");
        assert_eq!(full, "app.my_seq");

        let bad = ObjectName(vec![Ident::new("a"), Ident::new("b"), Ident::new("c")]);
        let err = normalize_sequence_name(&bad, &["public".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid sequence name"));
    }

    // Boundary keyword tests from issue #1671 — validated against PG 17.
    // Ensures format_serial_sequence_name delegates to quoting::quote_ident
    // and produces PG-compatible quoting for all six boundary keywords.
    #[test]
    fn format_serial_sequence_name_pg_boundary_keywords() {
        use crate::sql::sequences::format_serial_sequence_name;

        // PG RESERVED_KEYWORD — must quote
        assert_eq!(format_serial_sequence_name("column", "s"), "\"column\".s");
        // PG TYPE_FUNC_NAME_KEYWORD — must quote
        assert_eq!(format_serial_sequence_name("cross", "s"), "\"cross\".s");
        // PG RESERVED_KEYWORD — must quote
        assert_eq!(format_serial_sequence_name("select", "s"), "\"select\".s");
        // Not a PG keyword — must NOT quote
        assert_eq!(format_serial_sequence_name("tables", "s"), "tables.s");
        // PG UNRESERVED_KEYWORD — must NOT quote
        assert_eq!(format_serial_sequence_name("view", "s"), "view.s");
        // Not a PG keyword — must NOT quote
        assert_eq!(format_serial_sequence_name("name", "s"), "name.s");
    }

    /// P0-2 regression: `format_nextval_default` must quote mixed-case identifiers
    /// so that `parse_sequence_name_token` preserves case during resolution.
    #[test]
    fn format_nextval_default_quotes_mixed_case() {
        use crate::sql::sequences::format_nextval_default;

        // Lowercase names need no quoting.
        assert_eq!(
            format_nextval_default("public.t_id_seq"),
            "nextval('public.t_id_seq')"
        );
        // Mixed-case sequence name must be double-quoted inside the regclass string.
        assert_eq!(
            format_nextval_default("public.MyTable_MyCol_seq"),
            "nextval('public.\"MyTable_MyCol_seq\"')"
        );
        // Schema with reserved keyword must be quoted.
        assert_eq!(
            format_nextval_default("select.t_id_seq"),
            "nextval('\"select\".t_id_seq')"
        );
    }

    /// P1-1 regression: `resolve_serial_display_default` must propagate errors
    /// from `find_owned_sequence_full_name` (e.g. multiple owned sequences)
    /// instead of swallowing them with `.ok().flatten()`.
    #[test]
    fn resolve_serial_display_default_propagates_multiple_owned_error() {
        use crate::model::ColumnDef;
        use crate::sql::sequences::resolve_serial_display_default;

        let col = ColumnDef {
            name: "id".to_string(),
            data_type: crate::model::DataType::Int32,
            nullable: false,
            primary_key: true,
            unique: true,
            is_serial: true,
            default_expr: None, // ImplicitSequence behavior
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
            is_dropped: false,
        };
        let sequences = vec![
            make_sequence("public.s1", Some(("public.t", "id"))),
            make_sequence("public.s2", Some(("public.t", "id"))),
        ];
        let err = resolve_serial_display_default(&col, &sequences, "public.t", "public", "t")
            .unwrap_err();
        assert!(err.to_string().contains("Multiple sequences"));
    }

    #[test]
    fn expression_async_detection_flags_sequence_current_schema_and_unknown_function() {
        use crate::sql::sequences::{
            expr_needs_async_eval, expr_uses_current_schema, expr_uses_sequence_functions,
        };
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        fn first_expr(sql: &str) -> sqlparser::ast::Expr {
            let dialect = PostgreSqlDialect {};
            let ast = Parser::parse_sql(&dialect, sql).unwrap();
            match &ast[0] {
                sqlparser::ast::Statement::Query(q) => match q.body.as_ref() {
                    sqlparser::ast::SetExpr::Select(s) => match &s.projection[0] {
                        sqlparser::ast::SelectItem::UnnamedExpr(e) => e.clone(),
                        _ => panic!("expected unnamed expr"),
                    },
                    _ => panic!("expected select"),
                },
                _ => panic!("expected query"),
            }
        }

        let nextval = first_expr("SELECT nextval('public.s')");
        assert!(expr_uses_sequence_functions(&nextval));
        assert!(expr_needs_async_eval(&nextval));

        let lastval = first_expr("SELECT lastval()");
        assert!(expr_uses_sequence_functions(&lastval));
        assert!(expr_needs_async_eval(&lastval));

        let serial_seq = first_expr("SELECT pg_get_serial_sequence('public.t', 'id')");
        assert!(expr_uses_sequence_functions(&serial_seq));
        assert!(expr_needs_async_eval(&serial_seq));

        let serial_seq_mixed = first_expr("SELECT Pg_GeT_SeRiAl_SeQuEnCe('public.t', 'id')");
        assert!(expr_needs_async_eval(&serial_seq_mixed));

        let current_schema = first_expr("SELECT current_schema()");
        assert!(expr_uses_current_schema(&current_schema));
        assert!(expr_needs_async_eval(&current_schema));

        let unknown = first_expr("SELECT user_defined_fn(1)");
        assert!(expr_needs_async_eval(&unknown));

        let builtin = first_expr("SELECT abs(-1)");
        assert!(!expr_needs_async_eval(&builtin));
    }
}
