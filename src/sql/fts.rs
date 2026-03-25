use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

use super::fts_tokenizers::{get_tokenizer, resolve_user_tsc, TokenizerFn};

mod headline;
mod tsquery;

pub(crate) use headline::*;
pub(crate) use tsquery::*;

/// Resolve a tokenizer by config name.
///
/// Tries the static built-in registry first; falls back to the per-tenant
/// user-defined TSC cache (populated by `CREATE TEXT SEARCH CONFIGURATION`).
fn resolve_tokenizer(config: &str) -> Option<TokenizerFn> {
    if let Some(f) = get_tokenizer(config) {
        return Some(f);
    }
    // Fallback: check user-defined text search configurations.
    let ks = crate::session_context::current_keyspace();
    let db_id = crate::session_context::current_database_id();
    resolve_user_tsc(&ks, db_id, config)
}

/// Read the current session's `default_text_search_config` (session-aware).
fn session_config() -> String {
    crate::session_context::current_text_search_config().to_string()
}

fn is_english_like_config(config: &str) -> bool {
    config.eq_ignore_ascii_case("simple")
        || config.eq_ignore_ascii_case("english")
        || config.eq_ignore_ascii_case("english_stem")
}

fn has_stopwords(config: &str) -> bool {
    config.eq_ignore_ascii_case("english") || config.eq_ignore_ascii_case("english_stem")
}

fn is_simple_stopword(token: &str) -> bool {
    matches!(token, "a" | "an" | "is" | "the")
}

/// Return the appropriate stopword predicate for a given config.
///
/// - `english_stem`: full PG-compatible Snowball stopword list (~174 words)
/// - `english`: minimal compatibility list
/// - `simple`: no stopword dictionary (PostgreSQL-compatible)
/// - everything else: no stopword filtering
fn is_stopword_for_config(config: &str, word: &str) -> bool {
    if config.eq_ignore_ascii_case("english_stem") {
        super::fts_stopwords::is_english_stopword(word)
    } else if has_stopwords(config) {
        is_simple_stopword(word)
    } else {
        false
    }
}

pub fn to_tsvector(args: Vec<Value>) -> Result<Value> {
    let (config_owned, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (session_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.clone(),
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
    let config = config_owned.as_str();

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let tokens = tokenizer(&text);
    let tsvector = if is_english_like_config(config) {
        // Keep original token positions (including skipped stopwords) to match
        // PostgreSQL-style position numbering semantics.
        let mut entries: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, word) in tokens.into_iter().enumerate() {
            if is_stopword_for_config(config, &word) {
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
        // Non-English configs (CJK etc.) — use BTreeMap to deduplicate and
        // track positions, matching English path behavior.  No weight suffix
        // (default D in PG = no suffix).
        let mut entries: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, word) in tokens.into_iter().enumerate() {
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

fn extract_text_or_tsquery(value: &Value) -> Result<Option<String>> {
    match value {
        Value::Text(s) => Ok(Some(s.clone())),
        Value::Tsquery(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        _ => Err(anyhow::anyhow!("expected text or tsquery")),
    }
}

pub fn plainto_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config_owned, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (session_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.clone(),
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
    let config = config_owned.as_str();

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let tokens = tokenizer(&text);
    let filtered_tokens: Vec<String> = if is_english_like_config(config) {
        tokens
            .into_iter()
            .filter(|word| !is_stopword_for_config(config, word))
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

pub fn phraseto_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config_owned, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (session_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.clone(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, text)
        }
        _ => return Err(anyhow::anyhow!("phraseto_tsquery takes 1 or 2 arguments")),
    };
    let config = config_owned.as_str();

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    // Tokenize — positions are 1-based indices into the token array
    let tokens = tokenizer(&text);

    // Collect non-stopword tokens with their original positions
    let mut surviving: Vec<(usize, String)> = Vec::new();
    for (i, word) in tokens.into_iter().enumerate() {
        if !is_stopword_for_config(config, &word) {
            surviving.push((i + 1, word));
        }
    }

    if surviving.is_empty() {
        return Ok(Value::Tsquery(String::new()));
    }

    if surviving.len() == 1 {
        return Ok(Value::Tsquery(format!("'{}'", surviving[0].1)));
    }

    // Build phrase query with exact distances between surviving terms
    let mut parts = Vec::with_capacity(surviving.len() * 2 - 1);
    parts.push(format!("'{}'", surviving[0].1));
    for pair in surviving.windows(2) {
        let distance = pair[1].0 - pair[0].0;
        if distance == 1 {
            parts.push(" <-> ".to_string());
        } else {
            parts.push(format!(" <{}> ", distance));
        }
        parts.push(format!("'{}'", pair[1].1));
    }

    Ok(Value::Tsquery(parts.join("")))
}

pub fn websearch_to_tsquery(args: Vec<Value>) -> Result<Value> {
    let (config_owned, text) = match args.len() {
        1 => {
            let text = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (session_config(), text)
        }
        2 => {
            let config = match &args[0] {
                Value::Text(s) => s.clone(),
                _ => return Err(anyhow::anyhow!("first argument must be text search config")),
            };
            let text = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, text)
        }
        _ => {
            return Err(anyhow::anyhow!(
                "websearch_to_tsquery takes 1 or 2 arguments"
            ))
        }
    };
    let config = config_owned.as_str();

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let segments = parse_websearch_input(&text);

    let mut tsquery_parts: Vec<String> = Vec::new();

    for seg in segments {
        match seg {
            WebSearchSegment::Or => {
                if tsquery_parts.last().is_some_and(|part| part.trim() == "|") {
                    // Repeated OR keyword in the middle is treated as a term.
                    append_with_implicit_and(&mut tsquery_parts, "'or'");
                } else if !tsquery_parts.is_empty() {
                    tsquery_parts.push(" | ".to_string());
                }
            }
            WebSearchSegment::Word(w) => {
                let tokens = tokenizer(&w);
                let filtered: Vec<&String> = tokens
                    .iter()
                    .filter(|t| !is_stopword_for_config(config, t))
                    .collect();
                if let Some(first) = filtered.first() {
                    append_with_implicit_and(&mut tsquery_parts, &format!("'{}'", first));
                }
            }
            WebSearchSegment::NegatedWord(w) => {
                let tokens = tokenizer(&w);
                let filtered: Vec<&String> = tokens
                    .iter()
                    .filter(|t| !is_stopword_for_config(config, t))
                    .collect();
                if let Some(first) = filtered.first() {
                    append_with_implicit_and(&mut tsquery_parts, &format!("!'{}'", first));
                }
            }
            WebSearchSegment::Phrase(words) => {
                if let Some(phrase) = build_phrase_tsquery(&words, tokenizer, config) {
                    append_with_implicit_and(&mut tsquery_parts, &phrase);
                }
            }
            WebSearchSegment::NegatedPhrase(words) => {
                if let Some(phrase) = build_phrase_tsquery(&words, tokenizer, config) {
                    append_with_implicit_and(&mut tsquery_parts, &format!("!({})", phrase));
                }
            }
        }
    }

    // Clean up trailing operator
    while tsquery_parts
        .last()
        .is_some_and(|s| s.trim() == "|" || s.trim() == "&")
    {
        tsquery_parts.pop();
    }

    Ok(Value::Tsquery(tsquery_parts.join("")))
}

fn build_phrase_tsquery(words: &[String], tokenizer: TokenizerFn, config: &str) -> Option<String> {
    let mut all_tokens_with_pos: Vec<(usize, String)> = Vec::new();
    let mut pos = 0usize;
    for word in words {
        let tokens = tokenizer(word);
        if tokens.is_empty() {
            pos += 1;
            continue;
        }
        for tok in tokens {
            pos += 1;
            if !is_stopword_for_config(config, &tok) {
                all_tokens_with_pos.push((pos, tok));
            }
        }
    }

    if all_tokens_with_pos.is_empty() {
        return None;
    }

    let mut phrase = format!("'{}'", all_tokens_with_pos[0].1);
    for pair in all_tokens_with_pos.windows(2) {
        let distance = pair[1].0 - pair[0].0;
        if distance == 1 {
            phrase.push_str(" <-> ");
        } else {
            phrase.push_str(&format!(" <{}> ", distance));
        }
        phrase.push_str(&format!("'{}'", pair[1].1));
    }

    Some(phrase)
}

/// Append a term/phrase to tsquery parts, inserting implicit AND if needed.
fn append_with_implicit_and(parts: &mut Vec<String>, term: &str) {
    if let Some(last) = parts.last() {
        let trimmed = last.trim();
        if trimmed != "|" && !trimmed.is_empty() {
            parts.push(" & ".to_string());
        }
    }
    parts.push(term.to_string());
}

#[derive(Debug)]
enum WebSearchSegment {
    Word(String),
    NegatedWord(String),
    Phrase(Vec<String>),
    NegatedPhrase(Vec<String>),
    Or,
}

fn parse_websearch_quoted_words<I>(chars: &mut std::iter::Peekable<I>) -> Vec<String>
where
    I: Iterator<Item = char>,
{
    let mut phrase_words = Vec::new();
    let mut word = String::new();
    loop {
        match chars.next() {
            Some('"') => break,
            Some(c) if c.is_whitespace() => {
                if !word.is_empty() {
                    phrase_words.push(std::mem::take(&mut word));
                }
            }
            Some(c) => word.push(c),
            None => break,
        }
    }
    if !word.is_empty() {
        phrase_words.push(word);
    }
    phrase_words
}

/// Parse websearch input into segments. Never errors.
fn parse_websearch_input(input: &str) -> Vec<WebSearchSegment> {
    let mut segments = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next();
                let phrase_words = parse_websearch_quoted_words(&mut chars);
                if !phrase_words.is_empty() {
                    segments.push(WebSearchSegment::Phrase(phrase_words));
                }
            }
            '-' => {
                chars.next();
                if chars.peek().is_some_and(|c| *c == '"') {
                    chars.next();
                    let phrase_words = parse_websearch_quoted_words(&mut chars);
                    if !phrase_words.is_empty() {
                        segments.push(WebSearchSegment::NegatedPhrase(phrase_words));
                    }
                } else {
                    let mut word = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() || c == '"' {
                            break;
                        }
                        word.push(c);
                        chars.next();
                    }
                    if !word.is_empty() {
                        segments.push(WebSearchSegment::NegatedWord(word));
                    }
                }
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '"' {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                if word.eq_ignore_ascii_case("or") {
                    segments.push(WebSearchSegment::Or);
                } else if !word.is_empty() {
                    segments.push(WebSearchSegment::Word(word));
                }
            }
        }
    }

    let mut leading = 0usize;
    while leading < segments.len() && matches!(segments[leading], WebSearchSegment::Or) {
        segments[leading] = WebSearchSegment::Word("or".to_string());
        leading += 1;
    }
    let mut trailing = segments.len();
    while trailing > 0 && matches!(segments[trailing - 1], WebSearchSegment::Or) {
        segments[trailing - 1] = WebSearchSegment::Word("or".to_string());
        trailing -= 1;
    }

    segments
}

#[cfg(test)]
mod tests;
