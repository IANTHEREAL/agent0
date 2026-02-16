use crate::types::Value;
use anyhow::Result;
use std::collections::HashSet;

use super::fts_tokenizers::{default_text_search_config, get_tokenizer};

pub fn to_tsvector(args: Vec<Value>) -> Result<Value> {
    let (config, text) = match args.len() {
        1 => {
            // Single argument: use default tokenizer
            let text = extract_text(&args[0])?;
            (default_text_search_config(), text)
        }
        2 => {
            // Two arguments: (config, text)
            let config = match &args[0] {
                Value::Text(s) => s.as_str(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = extract_text(&args[1])?;
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("to_tsvector takes 1 or 2 arguments")),
    };

    // Get the tokenizer for the specified config
    let tokenizer = get_tokenizer(config).ok_or_else(|| {
        anyhow::anyhow!("unknown text search configuration: {}", config)
    })?;

    // Tokenize the text
    let tokens = tokenizer(&text);

    // Format as tsvector: 'word':position:weight
    let tsvector = tokens
        .into_iter()
        .enumerate()
        .map(|(i, word)| format!("'{}':{}A", word, i + 1))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Value::Tsvector(tsvector))
}

/// Extract text from a Value, handling Null gracefully.
fn extract_text(value: &Value) -> Result<String> {
    match value {
        Value::Text(s) => Ok(s.clone()),
        Value::Null => Ok(String::new()),
        _ => Err(anyhow::anyhow!("argument must be text")),
    }
}

pub fn plainto_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config, text) = match args.len() {
        1 => {
            // Single argument: use default tokenizer
            let text = extract_text(&args[0])?;
            (default_text_search_config(), text)
        }
        2 => {
            // Two arguments: (config, text)
            let config = match &args[0] {
                Value::Text(s) => s.as_str(),
                _ => {
                    return Err(anyhow::anyhow!(
                        "first argument must be text search config"
                    ))
                }
            };
            let text = extract_text(&args[1])?;
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("plainto_tsquery takes 1 or 2 arguments")),
    };

    // Get the tokenizer for the specified config
    let tokenizer = get_tokenizer(config).ok_or_else(|| {
        anyhow::anyhow!("unknown text search configuration: {}", config)
    })?;

    // Tokenize the text
    let tokens = tokenizer(&text);

    // Format as tsquery with AND operator
    let tsquery = tokens
        .into_iter()
        .map(|word| format!("'{}'", word))
        .collect::<Vec<_>>()
        .join(" & ");

    Ok(Value::Tsquery(tsquery))
}

pub fn to_tsquery(args: Vec<Value>) -> Result<Value> {
    let text = match args.len() {
        1 => {
            // Single argument: pass through as tsquery
            extract_text(&args[0])?
        }
        2 => {
            // Two arguments: (config, text) - config is for compatibility, pass through text
            // In PostgreSQL, to_tsquery respects config but we just pass through for now
            extract_text(&args[1])?
        }
        _ => return Err(anyhow::anyhow!("to_tsquery takes 1 or 2 arguments")),
    };

    Ok(Value::Tsquery(text))
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
    let matches = match_tsquery(&tsvector_words, tsquery_str);

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

fn match_tsquery(tsvector_words: &HashSet<String>, tsquery: &str) -> bool {
    let query_terms: Vec<String> = tsquery
        .split(|c: char| matches!(c, '&' | '|' | '!'))
        .map(|s| s.trim().trim_matches('\'').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    if query_terms.is_empty() {
        return true;
    }

    if tsquery.contains('|') {
        query_terms.iter().any(|term| tsvector_words.contains(term))
    } else {
        query_terms.iter().all(|term| tsvector_words.contains(term))
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
        .split(|c: char| matches!(c, '&' | '|' | '!'))
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

    matches as f64 / query_terms.len() as f64
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
    fn test_ts_rank() {
        let args = vec![
            Value::Tsvector("'hello':1A 'world':2A".to_string()),
            Value::Tsquery("'hello' & 'world'".to_string()),
        ];
        let result = ts_rank(args).unwrap();
        assert!(matches!(result, Value::Float64(r) if r > 0.0));
    }
}
