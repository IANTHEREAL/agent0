//! Unit tests for the PL/pgSQL module.

use super::utils::replace_identifier;

#[test]
fn test_replace_identifier_respects_word_boundaries() {
    assert_eq!(replace_identifier("n + 1", "n", "5"), "5 + 1");
    assert_eq!(replace_identifier("nn + n", "n", "5"), "nn + 5");
}

#[test]
fn test_replace_identifier_preserves_utf8() {
    let input = "'你好' || n";
    let output = replace_identifier(input, "n", "5");
    assert_eq!(output, "'你好' || 5");
}

#[test]
fn test_replace_identifier_skips_string_literals() {
    assert_eq!(
        replace_identifier("'negative' || n", "n", "5"),
        "'negative' || 5"
    );
    assert_eq!(
        replace_identifier("CASE WHEN n < 0 THEN 'negative' END", "n", "-5"),
        "CASE WHEN -5 < 0 THEN 'negative' END"
    );
    assert_eq!(
        replace_identifier("'it''s a test' || n", "n", "5"),
        "'it''s a test' || 5"
    );
}
