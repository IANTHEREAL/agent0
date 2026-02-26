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
}
