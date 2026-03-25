use super::*;

#[test]
fn test_to_tsvector() {
    let result = to_tsvector(vec![Value::Text("Hello World".to_string())]).unwrap();
    assert!(matches!(result, Value::Tsvector(_)));
}

#[test]
fn test_to_tsvector_simple_keeps_stopwords() {
    let result = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("a cat is the cat".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsvector(s) => {
            assert!(s.contains("'a':1"), "expected 'a' term, got: {}", s);
            assert!(
                s.contains("'cat':2,5"),
                "expected 'cat' positions, got: {}",
                s
            );
            assert!(s.contains("'is':3"), "expected 'is' term, got: {}", s);
            assert!(s.contains("'the':4"), "expected 'the' term, got: {}", s);
        }
        _ => panic!("Expected Tsvector"),
    }
}

#[test]
fn test_plainto_tsquery() {
    let result = plainto_tsquery(vec![Value::Text("hello world".to_string())]).unwrap();
    assert!(matches!(result, Value::Tsquery(_)));
}

#[test]
fn test_plainto_tsquery_simple_keeps_stopwords() {
    let result = plainto_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("a cat is the cat".to_string()),
    ])
    .unwrap();
    assert_eq!(
        result,
        Value::Tsquery("'a' & 'cat' & 'is' & 'the' & 'cat'".to_string())
    );
}

#[test]
fn test_ts_match() {
    let tsvector = Value::Tsvector("'hello':1A 'world':2A".to_string());
    let tsquery = Value::Tsquery("'hello' & 'world'".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(true));
}

#[test]
fn test_ts_match_no_match() {
    let tsvector = Value::Tsvector("'hello':1A 'world':2A".to_string());
    let tsquery = Value::Tsquery("'foo'".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(false));
}

#[test]
fn test_ts_match_not_operator() {
    let tsvector = Value::Tsvector("'hello':1A 'rust':2A".to_string());
    let tsquery = Value::Tsquery("'hello' & !'world'".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(true));

    let tsvector = Value::Tsvector("'hello':1A 'world':2A".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(false));
}

#[test]
fn test_ts_match_parentheses_precedence() {
    let tsvector = Value::Tsvector("'hello':1A 'world':2A".to_string());
    let tsquery = Value::Tsquery("('hello' | 'rust') & !'world'".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(false));
}

#[test]
fn test_ts_match_invalid_tsquery_errors() {
    let tsvector = Value::Tsvector("'hello':1A 'world':2A".to_string());
    let tsquery = Value::Tsquery("'hello' & (".to_string());
    let err = ts_match(&tsvector, &tsquery).unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError for tsquery syntax");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("no operand in tsquery"));
}

#[test]
fn test_ts_match_syntax_error_leading_and() {
    let tsvector = Value::Tsvector("'foo':1A".to_string());
    let tsquery = Value::Tsquery("& foo".to_string());
    let err = ts_match(&tsvector, &tsquery).unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn test_ts_match_syntax_error_double_and() {
    let tsvector = Value::Tsvector("'foo':1A".to_string());
    let tsquery = Value::Tsquery("'foo' && 'bar'".to_string());
    let err = ts_match(&tsvector, &tsquery).unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn test_ts_match_syntax_error_leading_or() {
    let tsvector = Value::Tsvector("'foo':1A".to_string());
    let tsquery = Value::Tsquery("| foo".to_string());
    let err = ts_match(&tsvector, &tsquery).unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn test_ts_match_syntax_error_leading_rparen() {
    let tsvector = Value::Tsvector("'foo':1A".to_string());
    let tsquery = Value::Tsquery(") foo".to_string());
    let err = ts_match(&tsvector, &tsquery).unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError");
    assert_eq!(sql_err.sqlstate(), "42601");
    assert!(sql_err.to_string().contains("syntax error in tsquery"));
}

#[test]
fn test_ts_match_empty_tsquery_is_false() {
    let tsvector = Value::Tsvector("'hello':1A".to_string());
    let tsquery = Value::Tsquery("".to_string());
    let result = ts_match(&tsvector, &tsquery).unwrap();
    assert_eq!(result, Value::Boolean(false));
}

#[test]
fn test_ts_rank() {
    let args = vec![
        Value::Tsvector("'hello':1A 'world':2A".to_string()),
        Value::Tsquery("'hello' & 'world'".to_string()),
    ];
    let result = ts_rank(args).unwrap();
    assert!(matches!(result, Value::Float64(r) if r > 0.0));
}

#[test]
fn test_ts_rank_three_args_feature_not_supported() {
    let result = ts_rank(vec![
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
        Value::Int32(1),
    ])
    .unwrap();
    assert!(matches!(result, Value::Float64(r) if r > 0.0));
}

#[test]
fn test_ts_rank_cd_four_args_feature_not_supported() {
    let result = ts_rank_cd(vec![
        Value::Array(vec![
            Value::Float64(0.1),
            Value::Float64(0.2),
            Value::Float64(0.4),
            Value::Float64(1.0),
        ]),
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
        Value::Int32(1),
    ])
    .unwrap();
    assert!(matches!(result, Value::Float64(r) if r > 0.0));
}

#[test]
fn test_to_tsvector_null_returns_null() {
    let result = to_tsvector(vec![Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

#[test]
fn test_plainto_tsquery_null_returns_null() {
    let result = plainto_tsquery(vec![Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

#[test]
fn test_to_tsquery_null_returns_null() {
    let result = to_tsquery(vec![Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

#[test]
fn test_to_tsquery_with_config() {
    let result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("hello & world".to_string()),
    ])
    .unwrap();
    assert!(matches!(result, Value::Tsquery(_)));
}

#[test]
fn test_to_tsquery_preserves_or_operator() {
    // This is the key bug fix test - to_tsquery must preserve | operator
    let result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("cat | dog".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => {
            assert!(
                s.contains(" | "),
                "Expected OR operator in tsquery, got: {}",
                s
            );
            assert!(
                !s.contains(" & "),
                "Should not have AND operator, got: {}",
                s
            );
        }
        _ => panic!("Expected Tsquery value"),
    }
}

#[test]
fn test_to_tsquery_preserves_and_operator() {
    let result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("cat & dog".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => {
            assert!(
                s.contains(" & "),
                "Expected AND operator in tsquery, got: {}",
                s
            );
        }
        _ => panic!("Expected Tsquery value"),
    }
}

#[test]
fn test_to_tsquery_preserves_not_operator() {
    let result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("!cat".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => {
            assert!(
                s.contains("!"),
                "Expected NOT operator in tsquery, got: {}",
                s
            );
        }
        _ => panic!("Expected Tsquery value"),
    }
}

#[test]
fn test_to_tsquery_preserves_parentheses() {
    let result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("(cat | dog) & bird".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => {
            assert!(s.contains("("), "Expected open paren, got: {}", s);
            assert!(s.contains(")"), "Expected close paren, got: {}", s);
            assert!(s.contains(" | "), "Expected OR operator, got: {}", s);
            assert!(s.contains(" & "), "Expected AND operator, got: {}", s);
        }
        _ => panic!("Expected Tsquery value"),
    }
}

#[test]
fn test_to_tsquery_or_matches_correctly() {
    // Test that OR queries actually match correctly
    let tsvector = Value::Tsvector("'cat':1A".to_string());
    let tsquery_result = to_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("cat | dog".to_string()),
    ])
    .unwrap();

    let result = ts_match(&tsvector, &tsquery_result).unwrap();
    assert_eq!(result, Value::Boolean(true), "cat should match 'cat | dog'");

    // Test that dog also matches
    let tsvector_dog = Value::Tsvector("'dog':1A".to_string());
    let result_dog = ts_match(&tsvector_dog, &tsquery_result).unwrap();
    assert_eq!(
        result_dog,
        Value::Boolean(true),
        "dog should match 'cat | dog'"
    );

    // Test that bird does not match
    let tsvector_bird = Value::Tsvector("'bird':1A".to_string());
    let result_bird = ts_match(&tsvector_bird, &tsquery_result).unwrap();
    assert_eq!(
        result_bird,
        Value::Boolean(false),
        "bird should not match 'cat | dog'"
    );
}

#[test]
fn test_to_tsquery_english_filters_stopwords() {
    let result = to_tsquery(vec![
        Value::Text("english".to_string()),
        Value::Text("the & fat".to_string()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'fat'".to_string()));
}

#[test]
fn test_to_tsquery_invalid_trailing_operator_errors() {
    let err = to_tsquery(vec![
        Value::Text("english".to_string()),
        Value::Text("foo &".to_string()),
    ])
    .unwrap_err();
    assert!(
        err.to_string().contains("no operand in tsquery"),
        "unexpected error: {err}"
    );
}

// ── Task 0 / Prerequisite: position parser tests ──

#[test]
fn test_parse_position_list_single() {
    let result = parse_position_list("1");
    assert_eq!(result, vec![(1, 'D')]);
}

#[test]
fn test_parse_position_list_single_with_weight() {
    let result = parse_position_list("1A");
    assert_eq!(result, vec![(1, 'A')]);
}

#[test]
fn test_parse_position_list_multi() {
    let result = parse_position_list("1,3,5");
    assert_eq!(result, vec![(1, 'D'), (3, 'D'), (5, 'D')]);
}

#[test]
fn test_parse_position_list_multi_with_weights() {
    let result = parse_position_list("1A,3B,5");
    assert_eq!(result, vec![(1, 'A'), (3, 'B'), (5, 'D')]);
}

#[test]
fn test_parse_position_list_pg_setweight_format() {
    // PG's setweight produces '1A,3A,5A' format
    let result = parse_position_list("1A,3A,5A");
    assert_eq!(result, vec![(1, 'A'), (3, 'A'), (5, 'A')]);
}

#[test]
fn test_extract_tsvector_words_with_positions_english() {
    let map = extract_tsvector_words_with_positions("'hello':1 'world':2");
    assert_eq!(map.get("hello"), Some(&vec![(1, 'D')]));
    assert_eq!(map.get("world"), Some(&vec![(2, 'D')]));
}

#[test]
fn test_extract_tsvector_words_with_positions_multi() {
    let map = extract_tsvector_words_with_positions("'the':1,6 'cat':3 'sat':4");
    assert_eq!(map.get("the"), Some(&vec![(1, 'D'), (6, 'D')]));
    assert_eq!(map.get("cat"), Some(&vec![(3, 'D')]));
}

#[test]
fn test_extract_tsvector_words_with_positions_weighted() {
    let map = extract_tsvector_words_with_positions("'hello':1A 'world':2B");
    assert_eq!(map.get("hello"), Some(&vec![(1, 'A')]));
    assert_eq!(map.get("world"), Some(&vec![(2, 'B')]));
}

#[test]
fn test_extract_positions_only() {
    let map = extract_positions_only("'hello':1A,3B 'world':2");
    assert_eq!(map.get("hello"), Some(&vec![1, 3]));
    assert_eq!(map.get("world"), Some(&vec![2]));
}

#[test]
fn test_to_tsvector_no_weight_suffix() {
    // Task 0.3: non-English should NOT hardcode 'A' weight
    let result = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("hello world".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsvector(s) => {
            assert!(!s.contains('A'), "should not contain weight A, got: {}", s);
            assert!(s.contains("'hello':1"), "expected 'hello':1, got: {}", s);
            assert!(s.contains("'world':2"), "expected 'world':2, got: {}", s);
        }
        _ => panic!("Expected Tsvector"),
    }
}

#[test]
fn test_to_tsvector_simple_preserves_stopwords() {
    let result = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("a cat is here".to_string()),
    ])
    .unwrap();

    match result {
        Value::Tsvector(s) => {
            assert!(
                s.contains("'a':1"),
                "expected stopword 'a' in output: {}",
                s
            );
            assert!(
                s.contains("'cat':2"),
                "expected token 'cat' in output: {}",
                s
            );
            assert!(
                s.contains("'is':3"),
                "expected stopword 'is' in output: {}",
                s
            );
            assert!(
                s.contains("'here':4"),
                "expected token 'here' in output: {}",
                s
            );
        }
        other => panic!("Expected Tsvector, got {:?}", other),
    }
}

#[test]
fn test_to_tsvector_english_filters_stopwords() {
    let result = to_tsvector(vec![
        Value::Text("english".to_string()),
        Value::Text("a cat is here".to_string()),
    ])
    .unwrap();

    match result {
        Value::Tsvector(s) => {
            assert!(
                !s.contains("'a':"),
                "did not expect stopword 'a' in output: {}",
                s
            );
            assert!(
                !s.contains("'is':"),
                "did not expect stopword 'is' in output: {}",
                s
            );
            assert!(
                s.contains("'cat':2"),
                "expected token 'cat' in output: {}",
                s
            );
            assert!(
                s.contains("'here':4"),
                "expected token 'here' in output: {}",
                s
            );
        }
        other => panic!("Expected Tsvector, got {:?}", other),
    }
}

// ── Phrase operator tests ──────────────────────────────────────

#[test]
fn test_tokenize_tsquery_phrase_operator() {
    let tokens = tokenize_tsquery("'hello' <-> 'world'").unwrap();
    assert_eq!(tokens.len(), 3);
    assert!(matches!(tokens[0], TsQueryToken::Term(ref s) if s == "hello"));
    assert!(matches!(tokens[1], TsQueryToken::FollowedBy(1)));
    assert!(matches!(tokens[2], TsQueryToken::Term(ref s) if s == "world"));
}

#[test]
fn test_tokenize_tsquery_distance_operator() {
    let tokens = tokenize_tsquery("'hello' <2> 'world'").unwrap();
    assert_eq!(tokens.len(), 3);
    assert!(matches!(tokens[1], TsQueryToken::FollowedBy(2)));
}

#[test]
fn test_phrase_match_adjacent() {
    let tv = Value::Tsvector("'hello':1 'world':2".into());
    let tq = Value::Tsquery("'hello' <-> 'world'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_match_non_adjacent() {
    let tv = Value::Tsvector("'hello':1 'world':3".into());
    let tq = Value::Tsquery("'hello' <-> 'world'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_phrase_match_distance() {
    let tv = Value::Tsvector("'hello':1 'world':3".into());
    let tq = Value::Tsquery("'hello' <2> 'world'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_match_wrong_order() {
    let tv = Value::Tsvector("'world':1 'hello':2".into());
    let tq = Value::Tsquery("'hello' <-> 'world'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_phrase_chained() {
    let tv = Value::Tsvector("'quick':1 'brown':2 'fox':3".into());
    let tq = Value::Tsquery("'quick' <-> 'brown' <-> 'fox'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_chained_gap() {
    let tv = Value::Tsvector("'quick':1 'brown':2 'fox':4".into());
    let tq = Value::Tsquery("'quick' <-> 'brown' <-> 'fox'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_phrase_with_and() {
    let tv = Value::Tsvector("'quick':1 'brown':2 'fox':5".into());
    let tq = Value::Tsquery("'quick' <-> 'brown' & 'fox'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_with_and_fails_phrase() {
    let tv = Value::Tsvector("'quick':1 'brown':3 'fox':5".into());
    let tq = Value::Tsquery("'quick' <-> 'brown' & 'fox'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_phrase_with_grouped_or_matches() {
    let tv = Value::Tsvector("'a':1 'b':2 'c':3".into());
    let tq = Value::Tsquery("('a' | 'b') <-> 'c'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_with_grouped_negated_or_matches() {
    let tv = Value::Tsvector("'c':1".into());
    let tq = Value::Tsquery("(!'a' | !'b') <-> 'c'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_with_grouped_and_no_match() {
    let tv = Value::Tsvector("'a':1 'b':2 'c':3".into());
    let tq = Value::Tsquery("('a' & 'b') <-> 'c'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_phrase_negated_left() {
    let tv = Value::Tsvector("'dog':2".into());
    let tq = Value::Tsquery("!'cat' <-> 'dog'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(true));
}

#[test]
fn test_phrase_negated_left_fails() {
    let tv = Value::Tsvector("'cat':1 'dog':2".into());
    let tq = Value::Tsquery("!'cat' <-> 'dog'".into());
    assert_eq!(ts_match(&tv, &tq).unwrap(), Value::Boolean(false));
}

#[test]
fn test_to_tsquery_phrase_output() {
    let result = to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("'hello' <-> 'world'".into()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => assert!(s.contains("<->"), "expected <-> in output, got: {}", s),
        _ => panic!("Expected Tsquery, got: {:?}", result),
    }
}

#[test]
fn test_to_tsquery_distance_output() {
    let result = to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("'hello' <3> 'world'".into()),
    ])
    .unwrap();
    match result {
        Value::Tsquery(s) => assert!(s.contains("<3>"), "expected <3> in output, got: {}", s),
        _ => panic!("Expected Tsquery, got: {:?}", result),
    }
}

// ── phraseto_tsquery tests ─────────────────────────────────

#[test]
fn test_phraseto_tsquery_simple() {
    let result = phraseto_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' <-> 'world'".into()));
}

#[test]
fn test_phraseto_tsquery_simple_keeps_stopwords() {
    let result = phraseto_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("the cat is big".into()),
    ])
    .unwrap();
    assert_eq!(
        result,
        Value::Tsquery("'the' <-> 'cat' <-> 'is' <-> 'big'".into())
    );
}

#[test]
fn test_phraseto_tsquery_single_word() {
    let result = phraseto_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("cat".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'cat'".into()));
}

#[test]
fn test_phraseto_tsquery_all_stopwords() {
    let result = phraseto_tsquery(vec![
        Value::Text("english".into()),
        Value::Text("the".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery(String::new()));
}

#[test]
fn test_phraseto_tsquery_stopword_distance() {
    let result = phraseto_tsquery(vec![
        Value::Text("english".into()),
        Value::Text("the cat is big".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'cat' <2> 'big'".into()));
}

#[test]
fn test_phraseto_tsquery_adjacent_after_stopword() {
    let result = phraseto_tsquery(vec![
        Value::Text("english".into()),
        Value::Text("the fat cat".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'fat' <-> 'cat'".into()));
}

#[test]
fn test_phraseto_tsquery_null_returns_null() {
    let result = phraseto_tsquery(vec![Value::Text("simple".into()), Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

#[test]
fn test_phraseto_tsquery_three_words() {
    let result = phraseto_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("quick brown fox".into()),
    ])
    .unwrap();
    assert_eq!(
        result,
        Value::Tsquery("'quick' <-> 'brown' <-> 'fox'".into())
    );
}

// ── websearch_to_tsquery tests ─────────────────────────────

#[test]
fn test_websearch_simple_words() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' & 'world'".into()));
}

#[test]
fn test_websearch_simple_keeps_stopwords() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("the cat".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'the' & 'cat'".into()));
}

#[test]
fn test_websearch_quoted_phrase() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("\"hello world\"".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' <-> 'world'".into()));
}

#[test]
fn test_websearch_negation() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello -world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' & !'world'".into()));
}

#[test]
fn test_websearch_or_operator() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello or world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' | 'world'".into()));
}

#[test]
fn test_websearch_or_case_insensitive() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello OR world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' | 'world'".into()));
}

#[test]
fn test_websearch_repeated_or_treats_second_or_as_term() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello or or world".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' | 'or' & 'world'".into()));
}

#[test]
fn test_websearch_empty_input() {
    let result =
        websearch_to_tsquery(vec![Value::Text("simple".into()), Value::Text("".into())]).unwrap();
    assert_eq!(result, Value::Tsquery(String::new()));
}

#[test]
fn test_websearch_multiple_negations() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("-cat -dog".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("!'cat' & !'dog'".into()));
}

#[test]
fn test_websearch_mixed() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("quick \"brown fox\" -lazy".into()),
    ])
    .unwrap();
    assert_eq!(
        result,
        Value::Tsquery("'quick' & 'brown' <-> 'fox' & !'lazy'".into())
    );
}

#[test]
fn test_websearch_trailing_or_ignored() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello or".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'hello' & 'or'".into()));
}

#[test]
fn test_websearch_leading_or_ignored() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("or hello".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Tsquery("'or' & 'hello'".into()));
}

#[test]
fn test_websearch_negated_phrase() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("hello -\"world peace\"".into()),
    ])
    .unwrap();
    assert_eq!(
        result,
        Value::Tsquery("'hello' & !('world' <-> 'peace')".into())
    );
}

#[test]
fn test_websearch_never_errors() {
    let result = websearch_to_tsquery(vec![
        Value::Text("simple".into()),
        Value::Text("!@#$%^&*()".into()),
    ]);
    assert!(result.is_ok());
}

#[test]
fn test_websearch_null_returns_null() {
    let result = websearch_to_tsquery(vec![Value::Text("simple".into()), Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

// ── ts_headline tests ────────────────────────────────────

#[test]
fn test_ts_headline_basic() {
    let result = ts_headline(vec![
        Value::Text("the quick brown fox".into()),
        Value::Tsquery("'fox'".into()),
    ])
    .unwrap();
    match result {
        Value::Text(s) => {
            assert!(s.contains("<b>fox</b>"), "expected highlight, got: {}", s);
            assert!(s.contains("quick"), "should preserve non-matching words");
        }
        _ => panic!("Expected Text"),
    }
}

#[test]
fn test_ts_headline_three_arg_document_query_options() {
    let result = ts_headline(vec![
        Value::Text("hello world".into()),
        Value::Tsquery("'hello'".into()),
        Value::Text("StartSel=<em>, StopSel=</em>".into()),
    ])
    .unwrap();
    match result {
        Value::Text(s) => {
            assert!(
                s.contains("<em>hello</em>"),
                "expected 3-arg custom tags, got: {}",
                s
            );
        }
        _ => panic!("Expected Text"),
    }
}

#[test]
fn test_ts_headline_custom_tags() {
    let result = ts_headline(vec![
        Value::Text("simple".into()),
        Value::Text("hello world".into()),
        Value::Tsquery("'hello'".into()),
        Value::Text("StartSel=<em>, StopSel=</em>".into()),
    ])
    .unwrap();
    match result {
        Value::Text(s) => {
            assert!(
                s.contains("<em>hello</em>"),
                "expected custom tags, got: {}",
                s
            );
        }
        _ => panic!("Expected Text"),
    }
}

#[test]
fn test_ts_headline_no_match() {
    let result = ts_headline(vec![
        Value::Text("simple".into()),
        Value::Text("hello world".into()),
        Value::Tsquery("'xyz'".into()),
    ])
    .unwrap();
    assert_eq!(result, Value::Text("hello world".into()));
}

#[test]
fn test_ts_headline_null_returns_null() {
    let result = ts_headline(vec![Value::Text("hello world".into()), Value::Null]).unwrap();
    assert_eq!(result, Value::Null);
}

#[test]
fn test_ts_headline_multiple_matches() {
    let result = ts_headline(vec![
        Value::Text("simple".into()),
        Value::Text("cat and dog and cat".into()),
        Value::Tsquery("'cat'".into()),
    ])
    .unwrap();
    match result {
        Value::Text(s) => {
            let count = s.matches("<b>cat</b>").count();
            assert_eq!(count, 2, "expected 2 highlights, got: {}", s);
        }
        _ => panic!("Expected Text"),
    }
}

#[test]
fn test_ts_headline_three_args_document_query_options_accepts_tsquery() {
    let result = ts_headline(vec![
        Value::Text("cat and dog".into()),
        Value::Tsquery("'cat'".into()),
        Value::Text("StartSel=<em>, StopSel=</em>".into()),
    ])
    .unwrap();

    match result {
        Value::Text(s) => assert!(s.contains("<em>cat</em>"), "expected highlight, got: {}", s),
        other => panic!("Expected Text, got {:?}", other),
    }
}

#[test]
fn test_ts_headline_three_args_dispatches_by_type_not_document_value() {
    let result = ts_headline(vec![
        Value::Text("simple".into()),
        Value::Tsquery("'simple'".into()),
        Value::Text("StartSel=<em>, StopSel=</em>".into()),
    ])
    .unwrap();

    match result {
        Value::Text(s) => assert!(
            s.contains("<em>simple</em>"),
            "expected highlight, got: {}",
            s
        ),
        other => panic!("Expected Text, got {:?}", other),
    }
}

#[test]
fn test_ts_headline_three_text_args_rejected() {
    let err = ts_headline(vec![
        Value::Text("simple".into()),
        Value::Text("hello world".into()),
        Value::Text("hello".into()),
    ])
    .unwrap_err();
    let sql_err = err
        .downcast_ref::<SqlError>()
        .expect("expected typed SqlError");
    assert_eq!(sql_err.sqlstate(), "42883");
}

// ── Weight validation (PG getWeights parity) ──

#[test]
fn test_parse_weights_rejects_above_one() {
    // PG: weight > 1.0 → ERROR "weight out of range"
    let val = Value::Array(vec![
        Value::Float64(1.1),
        Value::Float64(0.2),
        Value::Float64(0.4),
        Value::Float64(1.0),
    ]);
    let err = parse_weights(&val, "ts_rank").unwrap_err();
    assert!(err.to_string().contains("weight out of range"));
}

#[test]
fn test_parse_weights_negative_falls_back_to_default() {
    // PG: negative → silently use default weight for that position
    let val = Value::Array(vec![
        Value::Float64(-0.5), // D default = 0.1
        Value::Float64(0.2),
        Value::Float64(0.4),
        Value::Float64(1.0),
    ]);
    let w = parse_weights(&val, "ts_rank").unwrap();
    assert!(
        (w[0] - 0.1).abs() < 1e-9,
        "negative weight should fallback to default 0.1"
    );
    assert!((w[1] - 0.2).abs() < 1e-9);
}

#[test]
fn test_parse_weights_nan_falls_back_to_default() {
    // PG: NaN fails >= 0 check → use default
    let val = Value::Array(vec![
        Value::Float64(f64::NAN),
        Value::Float64(0.2),
        Value::Float64(0.4),
        Value::Float64(1.0),
    ]);
    let w = parse_weights(&val, "ts_rank").unwrap();
    assert!(
        (w[0] - 0.1).abs() < 1e-9,
        "NaN weight should fallback to default 0.1"
    );
}

#[test]
fn test_parse_weights_zero_accepted() {
    // PG: 0.0 is >= 0, accepted as-is
    let val = Value::Array(vec![
        Value::Float64(0.0),
        Value::Float64(0.2),
        Value::Float64(0.4),
        Value::Float64(1.0),
    ]);
    let w = parse_weights(&val, "ts_rank").unwrap();
    assert!((w[0]).abs() < 1e-9, "zero weight should be accepted");
}

#[test]
fn test_parse_weights_exactly_one_accepted() {
    // PG: 1.0 is not > 1.0, accepted
    let val = Value::Array(vec![
        Value::Float64(1.0),
        Value::Float64(1.0),
        Value::Float64(1.0),
        Value::Float64(1.0),
    ]);
    assert!(parse_weights(&val, "ts_rank").is_ok());
}

#[test]
fn test_parse_weights_too_short() {
    let val = Value::Array(vec![Value::Float64(0.1), Value::Float64(0.2)]);
    let err = parse_weights(&val, "ts_rank").unwrap_err();
    assert!(err.to_string().contains("too short"));
}

// ── Normalization validation (PG int4 bitmask parity) ──

#[test]
fn test_parse_norm_negative_accepted() {
    // PG: -1 as int4 → 0xFFFFFFFF, all normalization flags set
    let norm = parse_norm(&Value::Int32(-1), "ts_rank").unwrap();
    assert_eq!(norm, u32::MAX);
}

#[test]
fn test_parse_norm_negative_bitmask() {
    // -2 as i32 → 0xFFFFFFFE → all flags except bit 0
    let norm = parse_norm(&Value::Int32(-2), "ts_rank").unwrap();
    assert_eq!(norm & 1, 0);
    assert_ne!(norm & 2, 0);
}

#[test]
fn test_parse_norm_rejects_int64() {
    // PG has no int8 overload for normalization
    let err = parse_norm(&Value::Int64(1), "ts_rank").unwrap_err();
    assert!(err.to_string().contains("must be integer"));
}

#[test]
fn test_ts_rank_with_negative_norm() {
    // Negative norm should work end-to-end (all flags active)
    let result = ts_rank(vec![
        Value::Tsvector("'hello':1A 'world':2B".to_string()),
        Value::Tsquery("'hello' & 'world'".to_string()),
        Value::Int32(-1),
    ]);
    assert!(result.is_ok());
    if let Ok(Value::Float64(r)) = result {
        assert!(
            (0.0..=1.0).contains(&r),
            "norm=32 clamps to rank/(rank+1) ≤ 1"
        );
    }
}

#[test]
fn test_ts_rank_weight_above_one_errors() {
    let result = ts_rank(vec![
        Value::Array(vec![
            Value::Float64(1.5),
            Value::Float64(0.2),
            Value::Float64(0.4),
            Value::Float64(1.0),
        ]),
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("weight out of range"));
}

#[test]
fn test_ts_rank_cd_negative_weights_use_defaults() {
    // With negative weights falling back to defaults, should produce same result as no weights
    let with_defaults = ts_rank_cd(vec![
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();

    let with_negative = ts_rank_cd(vec![
        Value::Array(vec![
            Value::Float64(-1.0),
            Value::Float64(-1.0),
            Value::Float64(-1.0),
            Value::Float64(-1.0),
        ]),
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();

    assert_eq!(
        with_defaults, with_negative,
        "all-negative weights should produce same result as default weights"
    );
}

// ── ts_rank term frequency tests (#1219) ──────────────────────

#[test]
fn test_ts_rank_term_frequency_direct() {
    // Direct tsvector strings — bypass to_tsvector, test ranking algorithm.
    // 3 positions must score higher than 1 position.
    let rank_3x = ts_rank(vec![
        Value::Tsvector("'database':1,2,3".to_string()),
        Value::Tsquery("'database'".to_string()),
    ])
    .unwrap();
    let rank_1x = ts_rank(vec![
        Value::Tsvector("'database':1".to_string()),
        Value::Tsquery("'database'".to_string()),
    ])
    .unwrap();
    match (&rank_3x, &rank_1x) {
        (Value::Float64(r3), Value::Float64(r1)) => {
            assert!(r3 > r1, "3x freq ({}) must be > 1x freq ({})", r3, r1);
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_ts_rank_term_frequency_via_to_tsvector_english() {
    // End-to-end via to_tsvector (English/simple config).
    let tv_3x = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("database database database".to_string()),
    ])
    .unwrap();
    let tv_1x = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("database".to_string()),
    ])
    .unwrap();
    let tq = plainto_tsquery(vec![
        Value::Text("simple".to_string()),
        Value::Text("database".to_string()),
    ])
    .unwrap();

    let rank_3x = ts_rank(vec![tv_3x, tq.clone()]).unwrap();
    let rank_1x = ts_rank(vec![tv_1x, tq]).unwrap();

    match (&rank_3x, &rank_1x) {
        (Value::Float64(r3), Value::Float64(r1)) => {
            assert!(
                r3 > r1,
                "English end-to-end: 3x ({}) must be > 1x ({})",
                r3,
                r1
            );
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_ts_rank_term_frequency_via_to_tsvector_chinese() {
    // End-to-end via to_tsvector (Chinese config).
    // Jieba must segment "数据库数据库数据库" into 3 separate "数据库" tokens.
    let tv_3x = to_tsvector(vec![
        Value::Text("chinese".to_string()),
        Value::Text("数据库数据库数据库".to_string()),
    ])
    .unwrap();
    let tv_1x = to_tsvector(vec![
        Value::Text("chinese".to_string()),
        Value::Text("数据库".to_string()),
    ])
    .unwrap();
    let tq = plainto_tsquery(vec![
        Value::Text("chinese".to_string()),
        Value::Text("数据库".to_string()),
    ])
    .unwrap();

    // Verify tsvector content
    match &tv_3x {
        Value::Tsvector(s) => assert!(
            s.contains(":1,2,3") || s.contains(":1,2,3"),
            "expected 3 positions in Chinese tsvector, got: {}",
            s
        ),
        _ => panic!("expected Tsvector"),
    }

    let rank_3x = ts_rank(vec![tv_3x, tq.clone()]).unwrap();
    let rank_1x = ts_rank(vec![tv_1x, tq]).unwrap();

    match (&rank_3x, &rank_1x) {
        (Value::Float64(r3), Value::Float64(r1)) => {
            assert!(
                r3 > r1,
                "Chinese end-to-end: 3x ({}) must be > 1x ({})",
                r3,
                r1
            );
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_ts_rank_normalization_flags() {
    let tv = Value::Tsvector("'hello':1 'world':2 'foo':3".to_string());
    let tq = Value::Tsquery("'hello'".to_string());

    let base = ts_rank(vec![tv.clone(), tq.clone()]).unwrap();
    for flag in [1, 2, 8, 16, 32] {
        let normed = ts_rank(vec![tv.clone(), tq.clone(), Value::Int32(flag)]).unwrap();
        match (&base, &normed) {
            (Value::Float64(b), Value::Float64(n)) => {
                assert!(
                    n < b,
                    "norm flag {} should reduce score: base={}, normed={}",
                    flag,
                    b,
                    n
                );
            }
            _ => panic!("expected Float64"),
        }
    }
}

#[test]
fn test_ts_rank_normalization_differentiates_by_frequency() {
    // With normalization flag 2 (LENGTH), different tsvectors produce
    // different normalized scores because cnt_length differs.
    let tv_3x = Value::Tsvector("'database':1,2,3".to_string());
    let tv_1x = Value::Tsvector("'database':1".to_string());
    let tq = Value::Tsquery("'database'".to_string());

    let norm_3x = ts_rank(vec![tv_3x, tq.clone(), Value::Int32(2)]).unwrap();
    let norm_1x = ts_rank(vec![tv_1x, tq, Value::Int32(2)]).unwrap();

    match (&norm_3x, &norm_1x) {
        (Value::Float64(n3), Value::Float64(n1)) => {
            assert!(
                (n3 - n1).abs() > 1e-10,
                "norm flag 2 should differentiate: 3x={}, 1x={}",
                n3,
                n1
            );
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_ts_rank_cd_term_frequency() {
    // ts_rank_cd should also respect term frequency.
    let rank_3x = ts_rank_cd(vec![
        Value::Tsvector("'hello':1,2,3".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();
    let rank_1x = ts_rank_cd(vec![
        Value::Tsvector("'hello':1".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();
    match (&rank_3x, &rank_1x) {
        (Value::Float64(r3), Value::Float64(r1)) => {
            assert!(r3 > r1, "ts_rank_cd: 3x ({}) must be > 1x ({})", r3, r1);
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_ts_rank_weight_a_higher_than_d() {
    let rank_a = ts_rank(vec![
        Value::Tsvector("'hello':1A".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();
    let rank_d = ts_rank(vec![
        Value::Tsvector("'hello':1".to_string()),
        Value::Tsquery("'hello'".to_string()),
    ])
    .unwrap();
    match (&rank_a, &rank_d) {
        (Value::Float64(ra), Value::Float64(rd)) => {
            assert!(ra > rd, "weight A ({}) must be > weight D ({})", ra, rd);
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_to_tsvector_english_repeated_positions() {
    let result = to_tsvector(vec![
        Value::Text("simple".to_string()),
        Value::Text("database database database".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsvector(s) => {
            assert_eq!(s, "'database':1,2,3");
        }
        _ => panic!("expected Tsvector"),
    }
}

#[test]
fn test_to_tsvector_chinese_repeated_positions() {
    let result = to_tsvector(vec![
        Value::Text("chinese".to_string()),
        Value::Text("数据库数据库数据库".to_string()),
    ])
    .unwrap();
    match result {
        Value::Tsvector(s) => {
            assert!(
                s.contains("'数据库':1,2,3"),
                "Chinese 3x should have positions 1,2,3, got: {}",
                s
            );
        }
        _ => panic!("expected Tsvector"),
    }
}
