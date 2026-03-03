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
            build_implicit_sequence_def, implicit_sequence_name, normalize_sequence_name,
        };
        use sqlparser::ast::{Ident, ObjectName};

        assert_eq!(implicit_sequence_name("t", "id"), "t_id_seq");

        let def = build_implicit_sequence_def("public.t", "id", &DataType::Int32);
        assert_eq!(def.schema, "public");
        assert_eq!(def.name, "t_id_seq");
        assert_eq!(def.max_value, i32::MAX as i64);

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

    /// Sentinel keys must never collide with real sequence cache keys, even when
    /// a schema is named identically to the sentinel prefix (e.g. `__lastval_seq_name__`).
    #[test]
    fn sentinel_keys_do_not_collide_with_real_sequence_keys() {
        use crate::sql::sequences::{
            get_lastval_sequence_name, set_lastval_sequence_name, LASTVAL_SENTINEL_KEY,
            LASTVAL_SEQUENCE_NAME_KEY,
        };
        use std::collections::HashMap;

        let mut map: HashMap<String, i64> = HashMap::new();

        // Simulate a real sequence in a schema whose name matches the old
        // (pre-fix) sentinel prefix.
        let colliding_key = "__lastval_seq_name__.my_seq";
        map.insert(colliding_key.to_string(), 42);

        // Also insert the sentinel value.
        map.insert(LASTVAL_SENTINEL_KEY.to_string(), 100);
        set_lastval_sequence_name(&mut map, "public.s1");

        // The real sequence entry must survive — it must NOT be removed by
        // `set_lastval_sequence_name`.
        assert_eq!(
            map.get(colliding_key).copied(),
            Some(42),
            "real sequence key was incorrectly removed by sentinel bookkeeping"
        );

        // The sentinel value must still be readable.
        assert_eq!(map.get(LASTVAL_SENTINEL_KEY).copied(), Some(100));

        // The tracked sequence name must be correct.
        assert_eq!(get_lastval_sequence_name(&map), Some("public.s1"));

        // Verify sentinel constants contain null bytes (structural invariant).
        assert!(
            LASTVAL_SENTINEL_KEY.contains('\0'),
            "LASTVAL_SENTINEL_KEY must contain null-byte delimiters"
        );
        assert!(
            LASTVAL_SEQUENCE_NAME_KEY.contains('\0'),
            "LASTVAL_SEQUENCE_NAME_KEY must contain null-byte delimiters"
        );
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

        let current_schema = first_expr("SELECT current_schema()");
        assert!(expr_uses_current_schema(&current_schema));
        assert!(expr_needs_async_eval(&current_schema));

        let unknown = first_expr("SELECT user_defined_fn(1)");
        assert!(expr_needs_async_eval(&unknown));

        let builtin = first_expr("SELECT abs(-1)");
        assert!(!expr_needs_async_eval(&builtin));
    }
}
