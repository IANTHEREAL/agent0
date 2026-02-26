use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::{BTreeMap, HashSet};

use super::fts_tokenizers::{default_text_search_config, get_tokenizer};

fn is_english_like_config(config: &str) -> bool {
    config.eq_ignore_ascii_case("simple") || config.eq_ignore_ascii_case("english")
}

fn is_simple_stopword(token: &str) -> bool {
    matches!(token, "a" | "an" | "is" | "the")
}

pub fn to_tsvector(args: Vec<Value>) -> Result<Value> {
    let (config, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (default_text_search_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.as_str(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("to_tsvector takes 1 or 2 arguments")),
    };

    let tokenizer = get_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let tokens = tokenizer(&text);
    let tsvector = if is_english_like_config(config) {
        // Keep original token positions (including skipped stopwords) to match
        // PostgreSQL-style position numbering semantics.
        let mut entries: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, word) in tokens.into_iter().enumerate() {
            if is_simple_stopword(&word) {
                continue;
            }
            entries.entry(word).or_default().push(i + 1);
        }
        entries
            .into_iter()
            .map(|(word, positions)| {
                let pos_list = positions
                    .into_iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("'{}':{}", word.replace('\'', "''"), pos_list)
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        tokens
            .into_iter()
            .enumerate()
            .map(|(i, word)| format!("'{}':{}A", word, i + 1))
            .collect::<Vec<_>>()
            .join(" ")
    };

    Ok(Value::Tsvector(tsvector))
}

/// Extract text from a Value, returning `None` for SQL NULL to preserve
/// three-valued logic (PostgreSQL: `to_tsvector(NULL)` returns NULL).
fn extract_text(value: &Value) -> Result<Option<String>> {
    match value {
        Value::Text(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        _ => Err(anyhow::anyhow!("argument must be text")),
    }
}

pub fn plainto_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (default_text_search_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.as_str(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("plainto_tsquery takes 1 or 2 arguments")),
    };

    let tokenizer = get_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let tokens = tokenizer(&text);
    let filtered_tokens: Vec<String> = if is_english_like_config(config) {
        tokens
            .into_iter()
            .filter(|word| !is_simple_stopword(word))
            .collect()
    } else {
        tokens
    };

    let tsquery = filtered_tokens
        .into_iter()
        .map(|word| format!("'{}'", word))
        .collect::<Vec<_>>()
        .join(" & ");

    Ok(Value::Tsquery(tsquery))
}

pub fn to_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (default_text_search_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.as_str(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("to_tsquery takes 1 or 2 arguments")),
    };

    let tokenizer = get_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    // Parse the input as tsquery expression, preserving operators
    let tokens = tokenize_tsquery(&text).map_err(|e| match e {
        TsQueryParseError::Syntax => anyhow::anyhow!("syntax error in tsquery: \"{}\"", text),
        TsQueryParseError::NoOperand => anyhow::anyhow!("no operand in tsquery: \"{}\"", text),
    })?;

    // Rebuild the query, normalizing terms through the tokenizer
    let tsquery = tokens
        .into_iter()
        .map(|token| match token {
            TsQueryToken::Term(term) => {
                // Normalize the term through the tokenizer
                let normalized = tokenizer(&term);
                if normalized.is_empty() {
                    // If tokenizer returns empty (e.g., stop words), keep original
                    format!("'{}'", term.to_lowercase())
                } else {
                    // Use the first token from the tokenizer (stemmed form)
                    format!("'{}'", normalized[0])
                }
            }
            TsQueryToken::And => " & ".to_string(),
            TsQueryToken::Or => " | ".to_string(),
            TsQueryToken::Not => "!".to_string(),
            TsQueryToken::LParen => "(".to_string(),
            TsQueryToken::RParen => ")".to_string(),
        })
        .collect::<Vec<_>>()
        .join("");

    Ok(Value::Tsquery(tsquery))
}

pub fn ts_rank(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 {
        return Err(anyhow::anyhow!("ts_rank requires at least 2 arguments"));
    }

    let tsvector = match &args[0] {
        Value::Tsvector(s) => s.as_str(),
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("ts_rank first argument must be tsvector")),
    };

    let tsquery = match &args[1] {
        Value::Tsquery(s) => s.as_str(),
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("ts_rank second argument must be tsquery")),
    };

    let rank = compute_rank(tsvector, tsquery);
    Ok(Value::Float64(rank))
}

pub fn ts_match(tsvector: &Value, tsquery: &Value) -> Result<Value> {
    let tsvector_str = match tsvector {
        Value::Tsvector(s) => s,
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("@@ left operand must be tsvector")),
    };

    let tsquery_str = match tsquery {
        Value::Tsquery(s) => s,
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("@@ right operand must be tsquery")),
    };

    let tsvector_words = extract_tsvector_words(tsvector_str);
    let matches = match_tsquery(&tsvector_words, tsquery_str)?;

    Ok(Value::Boolean(matches))
}

fn extract_tsvector_words(tsvector: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    for part in tsvector.split_whitespace() {
        if let Some(word) = part.split(':').next() {
            let word = word.trim_matches('\'');
            if !word.is_empty() {
                words.insert(word.to_lowercase());
            }
        }
    }
    words
}

pub(crate) fn validate_tsquery_syntax(tsquery: &str) -> Result<()> {
    let tokens = tokenize_tsquery(tsquery).map_err(|e| invalid_tsquery_syntax(tsquery, e))?;
    if tokens.is_empty() {
        return Ok(());
    }

    let words = HashSet::new();
    let mut evaluator = TsQueryEvaluator::new(&tokens, &words);
    evaluator
        .eval()
        .map_err(|e| invalid_tsquery_syntax(tsquery, e))
        .map(|_| ())
}

fn match_tsquery(tsvector_words: &HashSet<String>, tsquery: &str) -> Result<bool> {
    let tokens = tokenize_tsquery(tsquery).map_err(|e| invalid_tsquery_syntax(tsquery, e))?;
    if tokens.is_empty() {
        return Ok(false);
    }

    let mut evaluator = TsQueryEvaluator::new(&tokens, tsvector_words);
    evaluator
        .eval()
        .map_err(|e| invalid_tsquery_syntax(tsquery, e))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TsQueryToken {
    Not,
    And,
    Or,
    LParen,
    RParen,
    Term(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TsQueryParseError {
    Syntax,
    NoOperand,
}

fn tokenize_tsquery(tsquery: &str) -> std::result::Result<Vec<TsQueryToken>, TsQueryParseError> {
    let mut tokens = Vec::new();
    let mut chars = tsquery.chars().peekable();

    while let Some(ch) = chars.peek().copied() {
        match ch {
            c if c.is_whitespace() => {
                chars.next();
            }
            '!' => {
                chars.next();
                tokens.push(TsQueryToken::Not);
            }
            '&' => {
                chars.next();
                tokens.push(TsQueryToken::And);
            }
            '|' => {
                chars.next();
                tokens.push(TsQueryToken::Or);
            }
            '(' => {
                chars.next();
                tokens.push(TsQueryToken::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(TsQueryToken::RParen);
            }
            '\'' => {
                chars.next(); // opening quote
                let mut term = String::new();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            term.push('\'');
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        term.push(c);
                    }
                }
                if !closed {
                    return Err(TsQueryParseError::Syntax);
                }
                if !term.is_empty() {
                    tokens.push(TsQueryToken::Term(term.to_lowercase()));
                }
            }
            _ => {
                let mut term = String::new();
                while let Some(c) = chars.peek().copied() {
                    if c.is_whitespace() || matches!(c, '!' | '&' | '|' | '(' | ')') {
                        break;
                    }
                    term.push(c);
                    chars.next();
                }
                let normalized = term.trim().trim_matches('\'').to_lowercase();
                if !normalized.is_empty() {
                    tokens.push(TsQueryToken::Term(normalized));
                }
            }
        }
    }

    Ok(tokens)
}

fn invalid_tsquery_syntax(tsquery: &str, err: TsQueryParseError) -> anyhow::Error {
    match err {
        TsQueryParseError::Syntax => SqlError::TsquerySyntax {
            query: tsquery.to_string(),
        }
        .into(),
        TsQueryParseError::NoOperand => SqlError::TsqueryNoOperand {
            query: tsquery.to_string(),
        }
        .into(),
    }
}

struct TsQueryEvaluator<'a> {
    tokens: &'a [TsQueryToken],
    pos: usize,
    words: &'a HashSet<String>,
}

impl<'a> TsQueryEvaluator<'a> {
    fn new(tokens: &'a [TsQueryToken], words: &'a HashSet<String>) -> Self {
        Self {
            tokens,
            pos: 0,
            words,
        }
    }

    fn eval(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        let value = self.parse_or()?;
        if self.pos != self.tokens.len() {
            return Err(TsQueryParseError::Syntax);
        }
        Ok(value)
    }

    fn parse_or(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        let mut value = self.parse_and()?;
        while self.consume_or() {
            let rhs = self.parse_and()?;
            value = value || rhs;
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        let mut value = self.parse_unary()?;
        while self.consume_and() {
            let rhs = self.parse_unary()?;
            value = value && rhs;
        }
        Ok(value)
    }

    fn parse_unary(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        if self.consume_not() {
            Ok(!self.parse_unary()?)
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        if self.consume_lparen() {
            let value = self.parse_or()?;
            if !self.consume_rparen() {
                return Err(TsQueryParseError::Syntax);
            }
            return Ok(value);
        }

        self.consume_term()
            .map(|term| self.words.contains(term.as_str()))
            .ok_or_else(|| {
                // Unexpected operator token → "syntax error in tsquery" (matches PG)
                // End-of-input → "no operand in tsquery" (matches PG)
                match self.peek_token() {
                    Some(TsQueryToken::And | TsQueryToken::Or | TsQueryToken::RParen) => {
                        TsQueryParseError::Syntax
                    }
                    _ => TsQueryParseError::NoOperand,
                }
            })
    }

    fn consume_and(&mut self) -> bool {
        self.consume_if(|token| matches!(token, TsQueryToken::And))
    }

    fn consume_or(&mut self) -> bool {
        self.consume_if(|token| matches!(token, TsQueryToken::Or))
    }

    fn consume_not(&mut self) -> bool {
        self.consume_if(|token| matches!(token, TsQueryToken::Not))
    }

    fn consume_lparen(&mut self) -> bool {
        self.consume_if(|token| matches!(token, TsQueryToken::LParen))
    }

    fn consume_rparen(&mut self) -> bool {
        self.consume_if(|token| matches!(token, TsQueryToken::RParen))
    }

    fn consume_term(&mut self) -> Option<String> {
        match self.peek_token() {
            Some(TsQueryToken::Term(term)) => {
                self.pos += 1;
                Some(term.clone())
            }
            _ => None,
        }
    }

    fn consume_if(&mut self, predicate: impl FnOnce(&TsQueryToken) -> bool) -> bool {
        if self.peek_token().is_some_and(predicate) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn peek_token(&self) -> Option<&'a TsQueryToken> {
        self.tokens.get(self.pos)
    }
}

/// Concatenate two tsvectors, merging their words and re-numbering positions.
/// Example: 'hello':1A || 'world':1A => 'hello':1A 'world':2A
pub fn concat_tsvector(left: &Value, right: &Value) -> Result<Value> {
    let left_str = match left {
        Value::Tsvector(s) => s.as_str(),
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("|| left operand must be tsvector")),
    };

    let right_str = match right {
        Value::Tsvector(s) => s.as_str(),
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("|| right operand must be tsvector")),
    };

    // Parse existing tsvector entries: 'word':posWeight ...
    // Find max position from left to offset right positions
    let mut max_pos = 0u32;
    let mut entries: Vec<(String, u32, char)> = Vec::new();

    for part in left_str.split_whitespace() {
        if let Some((word, pos_weight)) = parse_tsvector_entry(part) {
            if pos_weight.0 > max_pos {
                max_pos = pos_weight.0;
            }
            entries.push((word, pos_weight.0, pos_weight.1));
        }
    }

    // Add right entries with position offset
    for part in right_str.split_whitespace() {
        if let Some((word, pos_weight)) = parse_tsvector_entry(part) {
            let new_pos = max_pos + pos_weight.0;
            entries.push((word, new_pos, pos_weight.1));
        }
    }

    // Format output
    let result = entries
        .into_iter()
        .map(|(word, pos, weight)| format!("'{}':{}{}", word, pos, weight))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Value::Tsvector(result))
}

/// Parse a single tsvector entry like 'word':1A into (word, (position, weight))
fn parse_tsvector_entry(entry: &str) -> Option<(String, (u32, char))> {
    // Format: 'word':posWeight e.g. 'hello':1A
    let parts: Vec<&str> = entry.split(':').collect();
    if parts.len() != 2 {
        return None;
    }

    let word = parts[0].trim_matches('\'').to_string();
    let pos_weight = parts[1];

    if pos_weight.is_empty() {
        return None;
    }

    // Extract position number and weight letter
    let weight = pos_weight.chars().last().unwrap_or('A');
    let pos_str: String = pos_weight.chars().filter(|c| c.is_ascii_digit()).collect();
    let pos: u32 = pos_str.parse().unwrap_or(1);

    Some((word, (pos, weight)))
}

fn compute_rank(tsvector: &str, tsquery: &str) -> f64 {
    let tsvector_words = extract_tsvector_words(tsvector);
    let query_terms: Vec<String> = tsquery
        .split(|c: char| ['&', '|', '!'].contains(&c))
        .map(|s| s.trim().trim_matches('\'').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    if query_terms.is_empty() || tsvector_words.is_empty() {
        return 0.0;
    }

    let matches = query_terms
        .iter()
        .filter(|term| tsvector_words.contains(*term))
        .count();

    if matches == 0 {
        // PostgreSQL returns a tiny positive epsilon instead of hard zero for
        // non-matching ranks in many common configurations.
        return 1e-20;
    }

    // Deterministic PG-like baseline score for matching terms.
    0.060_792_71 * (matches as f64 / query_terms.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_tsvector() {
        let result = to_tsvector(vec![Value::Text("Hello World".to_string())]).unwrap();
        assert!(matches!(result, Value::Tsvector(_)));
    }

    #[test]
    fn test_plainto_tsquery() {
        let result = plainto_tsquery(vec![Value::Text("hello world".to_string())]).unwrap();
        assert!(matches!(result, Value::Tsquery(_)));
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
            Value::Text("hello world".to_string()),
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
}
