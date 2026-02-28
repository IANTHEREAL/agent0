use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

use super::fts_tokenizers::{get_tokenizer, resolve_user_tsc, TokenizerFn};

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

pub fn ts_headline(args: Vec<Value>) -> Result<Value> {
    let (config_owned, document, query_str, options_str) = match args.len() {
        2 => {
            let doc = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            let q = match extract_text_or_tsquery(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (session_config(), doc, q, None)
        }
        3 => {
            if matches!(
                (&args[0], &args[1], &args[2]),
                (Value::Text(_), Value::Text(_), Value::Text(_))
            ) {
                return Err(
                    SqlError::FunctionNotFound("ts_headline(text, text, text)".into()).into(),
                );
            }
            if matches!(&args[1], Value::Tsquery(_)) {
                let doc = match extract_text(&args[0])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                let q = match extract_text_or_tsquery(&args[1])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                let opts = match extract_text(&args[2])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                (session_config(), doc, q, Some(opts))
            } else {
                let config = match extract_text(&args[0])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                let doc = match extract_text(&args[1])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                let q = match extract_text_or_tsquery(&args[2])? {
                    Some(t) => t,
                    None => return Ok(Value::Null),
                };
                (config, doc, q, None)
            }
        }
        4 => {
            let config = match extract_text(&args[0])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            let doc = match extract_text(&args[1])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            let q = match extract_text_or_tsquery(&args[2])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            let opts = match extract_text(&args[3])? {
                Some(t) => t,
                None => return Ok(Value::Null),
            };
            (config, doc, q, Some(opts))
        }
        _ => return Err(anyhow::anyhow!("ts_headline takes 2 to 4 arguments")),
    };
    let config = config_owned.as_str();
    let opts = parse_headline_options(options_str.as_deref());

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    let query_terms = extract_query_terms(&query_str);
    if query_terms.is_empty() {
        return Ok(Value::Text(document));
    }

    let word_spans = scan_word_spans(&document);

    let mut matching_spans: Vec<bool> = Vec::with_capacity(word_spans.len());
    for &(start, end) in &word_spans {
        let original = &document[start..end];
        let stemmed_tokens = tokenizer(original);
        let matches = stemmed_tokens
            .iter()
            .any(|tok| query_terms.contains(&tok.to_lowercase()));
        matching_spans.push(matches);
    }

    if opts.max_fragments == 0 {
        let result = highlight_full_document(&document, &word_spans, &matching_spans, &opts);
        Ok(Value::Text(result))
    } else {
        let result = highlight_fragments(&document, &word_spans, &matching_spans, &opts);
        Ok(Value::Text(result))
    }
}

struct HeadlineOptions {
    start_sel: String,
    stop_sel: String,
    max_words: usize,
    min_words: usize,
    #[allow(dead_code)]
    short_word: usize,
    #[allow(dead_code)]
    highlight_all: bool,
    max_fragments: usize,
    fragment_delimiter: String,
}

fn parse_headline_options(options: Option<&str>) -> HeadlineOptions {
    let mut opts = HeadlineOptions {
        start_sel: "<b>".to_string(),
        stop_sel: "</b>".to_string(),
        max_words: 35,
        min_words: 15,
        short_word: 3,
        highlight_all: false,
        max_fragments: 0,
        fragment_delimiter: " ... ".to_string(),
    };
    if let Some(options_str) = options {
        for part in options_str.split(',') {
            let part = part.trim();
            if let Some((key, value)) = part.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    k if k.eq_ignore_ascii_case("StartSel") => opts.start_sel = value.to_string(),
                    k if k.eq_ignore_ascii_case("StopSel") => opts.stop_sel = value.to_string(),
                    k if k.eq_ignore_ascii_case("MaxWords") => {
                        if let Ok(n) = value.parse() {
                            opts.max_words = n;
                        }
                    }
                    k if k.eq_ignore_ascii_case("MinWords") => {
                        if let Ok(n) = value.parse() {
                            opts.min_words = n;
                        }
                    }
                    k if k.eq_ignore_ascii_case("ShortWord") => {
                        if let Ok(n) = value.parse() {
                            opts.short_word = n;
                        }
                    }
                    k if k.eq_ignore_ascii_case("HighlightAll") => {
                        opts.highlight_all = value.eq_ignore_ascii_case("true") || value == "1";
                    }
                    k if k.eq_ignore_ascii_case("MaxFragments") => {
                        if let Ok(n) = value.parse() {
                            opts.max_fragments = n;
                        }
                    }
                    k if k.eq_ignore_ascii_case("FragmentDelimiter") => {
                        opts.fragment_delimiter = value.to_string()
                    }
                    _ => {}
                }
            }
        }
    }
    opts
}

fn extract_query_terms(tsquery: &str) -> std::collections::HashSet<String> {
    let mut terms = std::collections::HashSet::new();
    if let Ok(tokens) = tokenize_tsquery(tsquery) {
        for token in tokens {
            if let TsQueryToken::Term(t) = token {
                terms.insert(t.to_lowercase());
            }
        }
    }
    terms
}

fn scan_word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(i, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if is_cjk_char(ch) {
            spans.push((i, i + ch.len_utf8()));
            chars.next();
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' || ch == '\'' {
            let start = i;
            chars.next();
            while let Some(&(_, c)) = chars.peek() {
                if c.is_alphanumeric() || c == '_' || c == '\'' {
                    chars.next();
                } else {
                    break;
                }
            }
            let end = chars.peek().map_or(text.len(), |&(j, _)| j);
            spans.push((start, end));
        } else {
            chars.next();
        }
    }
    spans
}

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |
        '\u{3400}'..='\u{4DBF}' |
        '\u{F900}'..='\u{FAFF}' |
        '\u{3000}'..='\u{303F}' |
        '\u{3040}'..='\u{309F}' |
        '\u{30A0}'..='\u{30FF}' |
        '\u{AC00}'..='\u{D7AF}'
    )
}

fn highlight_full_document(
    document: &str,
    word_spans: &[(usize, usize)],
    matching_spans: &[bool],
    opts: &HeadlineOptions,
) -> String {
    let mut result = String::with_capacity(document.len() + 64);
    let mut last_end = 0;
    for (idx, &(start, end)) in word_spans.iter().enumerate() {
        result.push_str(&document[last_end..start]);
        if matching_spans[idx] {
            result.push_str(&opts.start_sel);
            result.push_str(&document[start..end]);
            result.push_str(&opts.stop_sel);
        } else {
            result.push_str(&document[start..end]);
        }
        last_end = end;
    }
    result.push_str(&document[last_end..]);
    result
}

fn highlight_fragments(
    document: &str,
    word_spans: &[(usize, usize)],
    matching_spans: &[bool],
    opts: &HeadlineOptions,
) -> String {
    if word_spans.is_empty() {
        return document.to_string();
    }
    let match_positions: Vec<usize> = matching_spans
        .iter()
        .enumerate()
        .filter_map(|(i, &m)| if m { Some(i) } else { None })
        .collect();
    if match_positions.is_empty() {
        let end_idx = opts.min_words.min(word_spans.len());
        let byte_end = word_spans[end_idx.saturating_sub(1)].1;
        return document[..byte_end].to_string();
    }
    let mut fragments: Vec<String> = Vec::new();
    let mut used = vec![false; word_spans.len()];
    let max_frags = opts.max_fragments;
    for &match_idx in &match_positions {
        if fragments.len() >= max_frags {
            break;
        }
        if used[match_idx] {
            continue;
        }
        let half = opts.max_words / 2;
        let frag_start = match_idx.saturating_sub(half);
        let frag_end = (match_idx + half + 1).min(word_spans.len());
        for used_slot in used.iter_mut().take(frag_end).skip(frag_start) {
            *used_slot = true;
        }
        let mut frag_text = String::new();
        let mut last = word_spans[frag_start].0;
        for i in frag_start..frag_end {
            let (ws, we) = word_spans[i];
            frag_text.push_str(&document[last..ws]);
            if matching_spans[i] {
                frag_text.push_str(&opts.start_sel);
                frag_text.push_str(&document[ws..we]);
                frag_text.push_str(&opts.stop_sel);
            } else {
                frag_text.push_str(&document[ws..we]);
            }
            last = we;
        }
        fragments.push(frag_text);
    }
    fragments.join(&opts.fragment_delimiter)
}

pub fn to_tsquery(args: Vec<Value>) -> Result<Value> {
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
        _ => return Err(anyhow::anyhow!("to_tsquery takes 1 or 2 arguments")),
    };
    let config = config_owned.as_str();

    let tokenizer = resolve_tokenizer(config)
        .ok_or_else(|| anyhow::anyhow!("unknown text search configuration: {}", config))?;

    // Parse the input as tsquery expression, preserving operators
    let tokens = tokenize_tsquery(&text).map_err(|e| match e {
        TsQueryParseError::Syntax => anyhow::anyhow!("syntax error in tsquery: \"{}\"", text),
        TsQueryParseError::NoOperand => anyhow::anyhow!("no operand in tsquery: \"{}\"", text),
    })?;
    validate_tsquery_tokens(&tokens).map_err(|e| match e {
        TsQueryParseError::Syntax => anyhow::anyhow!("syntax error in tsquery: \"{}\"", text),
        TsQueryParseError::NoOperand => anyhow::anyhow!("no operand in tsquery: \"{}\"", text),
    })?;

    // Rebuild the query, normalizing terms and dropping stopwords for configs
    // that own a stopword dictionary (e.g., english / english_stem).
    let normalized = normalize_tsquery_tokens(tokens, tokenizer, config);
    let tsquery = render_tsquery_tokens(&normalized);

    Ok(Value::Tsquery(tsquery))
}

fn validate_tsquery_tokens(tokens: &[TsQueryToken]) -> std::result::Result<(), TsQueryParseError> {
    let positions: HashMap<String, Vec<u32>> = HashMap::new();
    let mut evaluator = TsQueryEvaluator::new(tokens, &positions);
    evaluator.eval().map(|_| ())
}

fn normalize_tsquery_tokens(
    tokens: Vec<TsQueryToken>,
    tokenizer: TokenizerFn,
    config: &str,
) -> Vec<TsQueryToken> {
    let mut normalized = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            TsQueryToken::Term(term) => {
                if let Some(normalized_term) = normalize_tsquery_term(&term, tokenizer, config) {
                    normalized.push(TsQueryToken::Term(normalized_term));
                }
            }
            other => normalized.push(other),
        }
    }
    sanitize_tsquery_tokens(normalized)
}

fn normalize_tsquery_term(term: &str, tokenizer: TokenizerFn, config: &str) -> Option<String> {
    let normalized = tokenizer(term);
    let first = normalized.first().map(String::as_str).unwrap_or(term);
    if is_stopword_for_config(config, first) {
        None
    } else {
        Some(first.to_lowercase())
    }
}

fn sanitize_tsquery_tokens(tokens: Vec<TsQueryToken>) -> Vec<TsQueryToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut expect_operand = true;
    let mut open_parens = 0usize;

    for token in tokens {
        match token {
            TsQueryToken::Term(term) => {
                out.push(TsQueryToken::Term(term));
                expect_operand = false;
            }
            TsQueryToken::Not => {
                if expect_operand {
                    out.push(TsQueryToken::Not);
                }
            }
            TsQueryToken::LParen => {
                if expect_operand {
                    out.push(TsQueryToken::LParen);
                    open_parens += 1;
                    expect_operand = true;
                }
            }
            TsQueryToken::RParen => {
                if !expect_operand && open_parens > 0 {
                    out.push(TsQueryToken::RParen);
                    open_parens -= 1;
                    expect_operand = false;
                }
            }
            op @ (TsQueryToken::And | TsQueryToken::Or | TsQueryToken::FollowedBy(_)) => {
                if !expect_operand {
                    out.push(op);
                    expect_operand = true;
                }
            }
        }
    }

    while out.last().is_some_and(|t| {
        matches!(
            t,
            TsQueryToken::And
                | TsQueryToken::Or
                | TsQueryToken::FollowedBy(_)
                | TsQueryToken::Not
                | TsQueryToken::LParen
        )
    }) {
        if matches!(out.last(), Some(TsQueryToken::LParen)) && open_parens > 0 {
            open_parens -= 1;
        }
        out.pop();
    }

    out
}

fn render_tsquery_tokens(tokens: &[TsQueryToken]) -> String {
    let mut parts = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            TsQueryToken::Term(term) => parts.push(format!("'{}'", term)),
            TsQueryToken::And => parts.push(" & ".to_string()),
            TsQueryToken::Or => parts.push(" | ".to_string()),
            TsQueryToken::Not => parts.push("!".to_string()),
            TsQueryToken::FollowedBy(1) => parts.push(" <-> ".to_string()),
            TsQueryToken::FollowedBy(n) => parts.push(format!(" <{}> ", n)),
            TsQueryToken::LParen => parts.push("(".to_string()),
            TsQueryToken::RParen => parts.push(")".to_string()),
        }
    }
    parts.join("")
}

const DEFAULT_WEIGHTS: [f64; 4] = [0.1, 0.2, 0.4, 1.0];

fn weight_value(w: char, weights: &[f64; 4]) -> f64 {
    match w {
        'A' => weights[3],
        'B' => weights[2],
        'C' => weights[1],
        _ => weights[0],
    }
}

fn numeric_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float64(f) => Some(*f),
        Value::Int32(i) => Some(*i as f64),
        Value::Int64(i) => Some(*i as f64),
        Value::Numeric(d) => d.to_string().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_norm(val: &Value, func: &str) -> Result<u32> {
    // PG signature: int4 normalization bitmask.  Negative values are valid
    // (e.g. -1 = 0xFFFFFFFF sets all flag bits).  No int8 overload in PG.
    let raw = match val {
        Value::Int32(i) => *i,
        _ => {
            return Err(anyhow::anyhow!(
                "{func} normalization argument must be integer"
            ))
        }
    };
    // Bit-reinterpret i32 → u32 (Rust wrapping cast matches C behaviour)
    Ok(raw as u32)
}

fn parse_weights(val: &Value, func: &str) -> Result<[f64; 4]> {
    let Value::Array(items) = val else {
        return Err(anyhow::anyhow!("{func} weight argument must be real[]"));
    };
    if items.len() < 4 {
        return Err(anyhow::anyhow!("array of weight is too short"));
    }
    let mut out = [0.0f64; 4];
    for (idx, item) in items.iter().take(4).enumerate() {
        let Some(v) = numeric_to_f64(item) else {
            return Err(anyhow::anyhow!(
                "{func} weight array must contain numeric values"
            ));
        };
        // PG getWeights: negative / NaN → fallback to default weight
        // (IEEE 754: NaN >= 0.0 is false, so this handles both cases)
        out[idx] = if v >= 0.0 { v } else { DEFAULT_WEIGHTS[idx] };
        // PG getWeights: value > 1.0 → ERROR "weight out of range"
        if out[idx] > 1.0 {
            return Err(anyhow::anyhow!("weight out of range"));
        }
    }
    Ok(out)
}

fn extract_tsvector_text<'a>(val: &'a Value, func: &str) -> Result<Option<&'a str>> {
    match val {
        Value::Tsvector(s) | Value::Text(s) => Ok(Some(s.as_str())),
        Value::Null => Ok(None),
        _ => Err(anyhow::anyhow!("{func} first argument must be tsvector")),
    }
}

fn extract_tsquery_text<'a>(val: &'a Value, func: &str) -> Result<Option<&'a str>> {
    match val {
        Value::Tsquery(s) | Value::Text(s) => Ok(Some(s.as_str())),
        Value::Null => Ok(None),
        _ => Err(anyhow::anyhow!("{func} second argument must be tsquery")),
    }
}

pub fn ts_rank(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 {
        return Err(anyhow::anyhow!("ts_rank requires at least 2 arguments"));
    }
    let (weights, tsvector, tsquery, norm) = match args.len() {
        2 => {
            let Some(tsvector) = extract_tsvector_text(&args[0], "ts_rank")? else {
                return Ok(Value::Null);
            };
            let Some(tsquery) = extract_tsquery_text(&args[1], "ts_rank")? else {
                return Ok(Value::Null);
            };
            (DEFAULT_WEIGHTS, tsvector, tsquery, 0)
        }
        3 => {
            // Overloads:
            // - ts_rank(tsvector, tsquery, normalization)
            // - ts_rank(weights, tsvector, tsquery)
            if matches!(args[0], Value::Array(_)) {
                let weights = parse_weights(&args[0], "ts_rank")?;
                let Some(tsvector) = extract_tsvector_text(&args[1], "ts_rank")? else {
                    return Ok(Value::Null);
                };
                let Some(tsquery) = extract_tsquery_text(&args[2], "ts_rank")? else {
                    return Ok(Value::Null);
                };
                (weights, tsvector, tsquery, 0)
            } else {
                let Some(tsvector) = extract_tsvector_text(&args[0], "ts_rank")? else {
                    return Ok(Value::Null);
                };
                let Some(tsquery) = extract_tsquery_text(&args[1], "ts_rank")? else {
                    return Ok(Value::Null);
                };
                let norm = parse_norm(&args[2], "ts_rank")?;
                (DEFAULT_WEIGHTS, tsvector, tsquery, norm)
            }
        }
        4 => {
            let weights = parse_weights(&args[0], "ts_rank")?;
            let Some(tsvector) = extract_tsvector_text(&args[1], "ts_rank")? else {
                return Ok(Value::Null);
            };
            let Some(tsquery) = extract_tsquery_text(&args[2], "ts_rank")? else {
                return Ok(Value::Null);
            };
            let norm = parse_norm(&args[3], "ts_rank")?;
            (weights, tsvector, tsquery, norm)
        }
        _ => return Err(anyhow::anyhow!("ts_rank takes 2 to 4 arguments")),
    };
    let rank = compute_rank(tsvector, tsquery, norm, &weights);
    Ok(Value::Float64(
        format!("{}", rank as f32).parse::<f64>().unwrap_or(rank),
    ))
}

pub fn ts_rank_cd(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 {
        return Err(anyhow::anyhow!("ts_rank_cd requires at least 2 arguments"));
    }
    let (weights, tsvector, tsquery, norm) = match args.len() {
        2 => {
            let Some(tsvector) = extract_tsvector_text(&args[0], "ts_rank_cd")? else {
                return Ok(Value::Null);
            };
            let Some(tsquery) = extract_tsquery_text(&args[1], "ts_rank_cd")? else {
                return Ok(Value::Null);
            };
            (DEFAULT_WEIGHTS, tsvector, tsquery, 0)
        }
        3 => {
            if matches!(args[0], Value::Array(_)) {
                let weights = parse_weights(&args[0], "ts_rank_cd")?;
                let Some(tsvector) = extract_tsvector_text(&args[1], "ts_rank_cd")? else {
                    return Ok(Value::Null);
                };
                let Some(tsquery) = extract_tsquery_text(&args[2], "ts_rank_cd")? else {
                    return Ok(Value::Null);
                };
                (weights, tsvector, tsquery, 0)
            } else {
                let Some(tsvector) = extract_tsvector_text(&args[0], "ts_rank_cd")? else {
                    return Ok(Value::Null);
                };
                let Some(tsquery) = extract_tsquery_text(&args[1], "ts_rank_cd")? else {
                    return Ok(Value::Null);
                };
                let norm = parse_norm(&args[2], "ts_rank_cd")?;
                (DEFAULT_WEIGHTS, tsvector, tsquery, norm)
            }
        }
        4 => {
            let weights = parse_weights(&args[0], "ts_rank_cd")?;
            let Some(tsvector) = extract_tsvector_text(&args[1], "ts_rank_cd")? else {
                return Ok(Value::Null);
            };
            let Some(tsquery) = extract_tsquery_text(&args[2], "ts_rank_cd")? else {
                return Ok(Value::Null);
            };
            let norm = parse_norm(&args[3], "ts_rank_cd")?;
            (weights, tsvector, tsquery, norm)
        }
        _ => return Err(anyhow::anyhow!("ts_rank_cd takes 2 to 4 arguments")),
    };
    let rank = compute_rank_cd(tsvector, tsquery, norm, &weights);
    Ok(Value::Float64(
        format!("{}", rank as f32).parse::<f64>().unwrap_or(rank),
    ))
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

    let word_positions = extract_positions_only(tsvector_str);
    let matches = match_tsquery(&word_positions, tsquery_str)?;

    Ok(Value::Boolean(matches))
}

/// Parse a tsvector string into a map of word → sorted list of (position, weight).
///
/// Handles all formats:
///  - `'word':1`          → position 1, weight D (default)
///  - `'word':1A`         → position 1, weight A
///  - `'word':1,3,5`      → positions [1,3,5], all weight D
///  - `'word':1A,3B,5`    → position 1 weight A, position 3 weight B, position 5 weight D
fn extract_tsvector_words_with_positions(tsvector: &str) -> HashMap<String, Vec<(u32, char)>> {
    let mut map: HashMap<String, Vec<(u32, char)>> = HashMap::new();
    for part in tsvector.split_whitespace() {
        let Some(colon) = part.find(':') else {
            // No colon — word-only entry (rare, but handle gracefully)
            let word = part.trim_matches('\'').to_lowercase();
            if !word.is_empty() {
                map.entry(word).or_default();
            }
            continue;
        };
        let word = part[..colon].trim_matches('\'').to_lowercase();
        if word.is_empty() {
            continue;
        }
        let pos_part = &part[colon + 1..];
        let positions = parse_position_list(pos_part);
        map.entry(word).or_default().extend(positions);
    }
    // Sort positions for each word
    for positions in map.values_mut() {
        positions.sort_by_key(|&(p, _)| p);
    }
    map
}

/// Convenience wrapper: positions only (no weights), for phrase matching.
fn extract_positions_only(tsvector: &str) -> HashMap<String, Vec<u32>> {
    extract_tsvector_words_with_positions(tsvector)
        .into_iter()
        .map(|(word, pws)| (word, pws.into_iter().map(|(p, _)| p).collect()))
        .collect()
}

/// Parse a position list like `1A,3B,5` into `[(1,'A'), (3,'B'), (5,'D')]`.
fn parse_position_list(s: &str) -> Vec<(u32, char)> {
    let mut result = Vec::new();
    for segment in s.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        // Separate trailing weight letter [A-D] from digits
        let last = segment.as_bytes().last().copied().unwrap_or(b'0');
        let (num_part, weight) = if matches!(last, b'A' | b'B' | b'C' | b'D') {
            (&segment[..segment.len() - 1], last as char)
        } else {
            (segment, 'D') // default weight
        };
        if let Ok(pos) = num_part.parse::<u32>() {
            if pos > 0 {
                result.push((pos, weight));
            }
        }
    }
    result
}

pub(crate) fn validate_tsquery_syntax(tsquery: &str) -> Result<()> {
    let tokens = tokenize_tsquery(tsquery).map_err(|e| invalid_tsquery_syntax(tsquery, e))?;
    if tokens.is_empty() {
        return Ok(());
    }

    let words: HashMap<String, Vec<u32>> = HashMap::new();
    let mut evaluator = TsQueryEvaluator::new(&tokens, &words);
    evaluator
        .eval()
        .map_err(|e| invalid_tsquery_syntax(tsquery, e))
        .map(|_| ())
}

fn match_tsquery(word_positions: &HashMap<String, Vec<u32>>, tsquery: &str) -> Result<bool> {
    let tokens = tokenize_tsquery(tsquery).map_err(|e| invalid_tsquery_syntax(tsquery, e))?;
    if tokens.is_empty() {
        return Ok(false);
    }

    let mut evaluator = TsQueryEvaluator::new(&tokens, word_positions);
    evaluator
        .eval()
        .map_err(|e| invalid_tsquery_syntax(tsquery, e))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TsQueryToken {
    Not,
    And,
    Or,
    FollowedBy(u32), // <-> = FollowedBy(1), <N> = FollowedBy(N)
    LParen,
    RParen,
    Term(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TsQueryParseError {
    Syntax,
    NoOperand,
}

/// Shared tsquery tokenizer used by both runtime evaluation and GIN planner.
///
/// Parses operators: `&` (AND), `|` (OR), `!` (NOT), `<->` (phrase), `<N>` (distance),
/// `(` `)` (grouping), and `'...'` or bare-word terms.
pub(crate) fn tokenize_tsquery(
    tsquery: &str,
) -> std::result::Result<Vec<TsQueryToken>, TsQueryParseError> {
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
            '<' => {
                // Try to parse <-> or <N>
                chars.next(); // consume '<'
                if chars.peek() == Some(&'-') {
                    // Might be <->
                    chars.next(); // consume '-'
                    if chars.peek() == Some(&'>') {
                        chars.next(); // consume '>'
                        tokens.push(TsQueryToken::FollowedBy(1));
                    } else {
                        return Err(TsQueryParseError::Syntax);
                    }
                } else {
                    // Try <digits>
                    let mut digits = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_digit() {
                            digits.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if !digits.is_empty() && chars.peek() == Some(&'>') {
                        chars.next(); // consume '>'
                        let n: u32 = digits.parse().unwrap_or(0);
                        if n == 0 {
                            return Err(TsQueryParseError::Syntax); // <0> is invalid
                        }
                        tokens.push(TsQueryToken::FollowedBy(n));
                    } else {
                        return Err(TsQueryParseError::Syntax);
                    }
                }
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
                    if c.is_whitespace() || matches!(c, '!' | '&' | '|' | '(' | ')' | '<') {
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

// ── EvalResult: position-aware evaluation result ──────────────────────

/// Result of evaluating a tsquery sub-expression against a tsvector.
///
/// Carries both position information (for phrase matching) and a
/// negation flag (for `!term <-> term` handling).
struct EvalResult {
    /// Positions where the sub-expression matched.
    positions: Vec<u32>,
    /// Whether this result is negated (under a NOT operator).
    negated: bool,
    /// Boolean truth value of this sub-expression.
    matched: bool,
}

impl EvalResult {
    /// Is this a boolean match?
    fn is_match(&self) -> bool {
        self.matched
    }

    /// Create a result for a term lookup.
    fn term(mut positions: Vec<u32>) -> Self {
        positions.sort_unstable();
        positions.dedup();
        let matched = !positions.is_empty();
        Self {
            positions,
            negated: false,
            matched,
        }
    }

    /// Negate (flip NOT flag, preserving positions).
    fn negate(mut self) -> Self {
        self.negated = !self.negated;
        self.matched = !self.matched;
        self
    }

    /// Boolean-only result (for AND/OR reductions).
    fn boolean(matched: bool, positions: Vec<u32>) -> Self {
        Self {
            positions,
            negated: false,
            matched,
        }
    }

    fn combine_or(left: Self, right: Self) -> Self {
        let matched = left.is_match() || right.is_match();
        if !matched {
            return Self::boolean(false, Vec::new());
        }

        // If a matched negated branch participates, the OR result can be
        // position-agnostically true; preserve that so phrase evaluation can
        // treat it as a permissive operand.
        let matched_negated_branch =
            (left.negated && left.is_match()) || (right.negated && right.is_match());
        if matched_negated_branch {
            return Self {
                positions: Vec::new(),
                negated: true,
                matched: true,
            };
        }

        let mut positions = Vec::new();
        if left.is_match() {
            positions = union_positions(&positions, &left.positions);
        }
        if right.is_match() {
            positions = union_positions(&positions, &right.positions);
        }
        Self {
            positions,
            negated: false,
            matched: true,
        }
    }

    fn combine_and(left: Self, right: Self) -> Self {
        let matched = left.is_match() && right.is_match();
        if !matched {
            return Self::boolean(false, Vec::new());
        }

        let positions = match (left.negated, right.negated) {
            (false, false) => intersect_positions(&left.positions, &right.positions),
            // Preserve positive-side anchors under conjunction with a matched
            // negated branch (e.g. !a & b).
            (true, false) => right.positions.clone(),
            (false, true) => left.positions.clone(),
            (true, true) => Vec::new(),
        };

        let negated = positions.is_empty() && (left.negated || right.negated);
        Self {
            positions,
            negated,
            matched: true,
        }
    }
}

fn union_positions(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => {
                if out.last().copied() != Some(left[i]) {
                    out.push(left[i]);
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                if out.last().copied() != Some(right[j]) {
                    out.push(right[j]);
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if out.last().copied() != Some(left[i]) {
                    out.push(left[i]);
                }
                i += 1;
                j += 1;
            }
        }
    }
    while i < left.len() {
        if out.last().copied() != Some(left[i]) {
            out.push(left[i]);
        }
        i += 1;
    }
    while j < right.len() {
        if out.last().copied() != Some(right[j]) {
            out.push(right[j]);
        }
        j += 1;
    }
    out
}

fn intersect_positions(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                if out.last().copied() != Some(left[i]) {
                    out.push(left[i]);
                }
                i += 1;
                j += 1;
            }
        }
    }
    out
}

// ── TsQueryEvaluator ──────────────────────────────────────────────────
struct TsQueryEvaluator<'a> {
    tokens: &'a [TsQueryToken],
    pos: usize,
    word_positions: &'a HashMap<String, Vec<u32>>,
}
impl<'a> TsQueryEvaluator<'a> {
    fn new(tokens: &'a [TsQueryToken], word_positions: &'a HashMap<String, Vec<u32>>) -> Self {
        Self {
            tokens,
            pos: 0,
            word_positions,
        }
    }
    fn eval(&mut self) -> std::result::Result<bool, TsQueryParseError> {
        let result = self.parse_or()?;
        if self.pos != self.tokens.len() {
            return Err(TsQueryParseError::Syntax);
        }
        Ok(result.is_match())
    }

    /// Precedence level 1 (lowest): OR
    fn parse_or(&mut self) -> std::result::Result<EvalResult, TsQueryParseError> {
        let mut left = self.parse_and()?;
        while self.consume_if(|t| matches!(t, TsQueryToken::Or)) {
            let right = self.parse_and()?;
            left = EvalResult::combine_or(left, right);
        }
        Ok(left)
    }

    /// Precedence level 2: AND
    fn parse_and(&mut self) -> std::result::Result<EvalResult, TsQueryParseError> {
        let mut left = self.parse_phrase()?;
        while self.consume_if(|t| matches!(t, TsQueryToken::And)) {
            let right = self.parse_phrase()?;
            left = EvalResult::combine_and(left, right);
        }
        Ok(left)
    }

    /// Precedence level 3: PHRASE (<-> / <N>)
    fn parse_phrase(&mut self) -> std::result::Result<EvalResult, TsQueryParseError> {
        let mut left = self.parse_unary()?;
        while let Some(distance) = self.try_consume_followed_by() {
            let right = self.parse_unary()?;
            left = eval_phrase(left, right, distance);
        }
        Ok(left)
    }

    /// Precedence level 4 (highest): NOT / unary
    fn parse_unary(&mut self) -> std::result::Result<EvalResult, TsQueryParseError> {
        if self.consume_if(|t| matches!(t, TsQueryToken::Not)) {
            Ok(self.parse_unary()?.negate())
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<EvalResult, TsQueryParseError> {
        if self.consume_if(|t| matches!(t, TsQueryToken::LParen)) {
            let result = self.parse_or()?;
            if !self.consume_if(|t| matches!(t, TsQueryToken::RParen)) {
                return Err(TsQueryParseError::Syntax);
            }
            return Ok(result);
        }
        self.consume_term()
            .map(|term| {
                let positions = self
                    .word_positions
                    .get(term.as_str())
                    .cloned()
                    .unwrap_or_default();
                EvalResult::term(positions)
            })
            .ok_or_else(|| match self.peek_token() {
                Some(
                    TsQueryToken::And
                    | TsQueryToken::Or
                    | TsQueryToken::RParen
                    | TsQueryToken::FollowedBy(_),
                ) => TsQueryParseError::Syntax,
                _ => TsQueryParseError::NoOperand,
            })
    }

    fn try_consume_followed_by(&mut self) -> Option<u32> {
        match self.peek_token() {
            Some(TsQueryToken::FollowedBy(n)) => {
                let n = *n;
                self.pos += 1;
                Some(n)
            }
            _ => None,
        }
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

/// Evaluate a phrase operator: `left <N> right`.
///
/// For each position in `right`, check whether `left` has a position exactly
/// `distance` before it.  Handles negation on either side.
fn eval_phrase(left: EvalResult, right: EvalResult, distance: u32) -> EvalResult {
    let mut result_positions = Vec::new();

    if !left.negated && !right.negated {
        // Normal: find right positions where some left position is exactly `distance` before
        for &rp in &right.positions {
            if rp > distance {
                let target = rp - distance;
                if left.positions.binary_search(&target).is_ok() {
                    result_positions.push(rp);
                }
            }
        }
    } else if left.negated && !right.negated {
        // !left <N> right: for each right pos, check NO left pos satisfies the distance
        if left.positions.is_empty() && left.is_match() {
            // Term absent + negated = trivially true → all right positions pass
            result_positions = right.positions.clone();
        } else {
            for &rp in &right.positions {
                if rp > distance {
                    let target = rp - distance;
                    if left.positions.binary_search(&target).is_err() {
                        result_positions.push(rp);
                    }
                } else {
                    // rp <= distance: no valid left position possible, negation passes
                    result_positions.push(rp);
                }
            }
        }
    } else if !left.negated && right.negated {
        // left <N> !right: for each left pos, check NO right pos at left+distance
        if right.positions.is_empty() && right.is_match() {
            // Term absent + negated = trivially true → all left positions pass
            result_positions = left.positions.iter().map(|&lp| lp + distance).collect();
        } else {
            for &lp in &left.positions {
                let target = lp + distance;
                if right.positions.binary_search(&target).is_err() {
                    result_positions.push(target);
                }
            }
        }
    } else {
        // Both negated: !left <N> !right — pathological, return empty (PG behavior)
    }

    let matched = !result_positions.is_empty();
    EvalResult {
        positions: result_positions,
        negated: false,
        matched,
    }
}

/// Concatenate two tsvectors, merging their words and re-numbering positions.
/// Example: 'hello':1 || 'world':1 => 'hello':1 'world':2
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

    // Parse left side, find max position for offset
    let left_parsed = extract_tsvector_words_with_positions(left_str);
    let mut max_pos = 0u32;
    for positions in left_parsed.values() {
        for &(p, _) in positions {
            if p > max_pos {
                max_pos = p;
            }
        }
    }

    // Collect all entries as (word, pos, weight) tuples
    let mut entries: Vec<(String, u32, char)> = Vec::new();
    for (word, positions) in &left_parsed {
        for &(pos, weight) in positions {
            entries.push((word.clone(), pos, weight));
        }
    }

    let right_parsed = extract_tsvector_words_with_positions(right_str);
    for (word, positions) in &right_parsed {
        for &(pos, weight) in positions {
            entries.push((word.clone(), max_pos + pos, weight));
        }
    }

    // Sort by position for deterministic output
    entries.sort_by_key(|&(_, pos, _)| pos);

    // Format output — omit weight suffix for default D (matching PG)
    let result = entries
        .into_iter()
        .map(|(word, pos, weight)| {
            if weight == 'D' {
                format!("'{}':{}", word, pos)
            } else {
                format!("'{}':{}{}", word, pos, weight)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Value::Tsvector(result))
}

/// Determine if top-level operator is AND or PHRASE (for rank dispatch).
/// PG uses calc_rank_and when the root operator is AND or PHRASE, else calc_rank_or.
fn tsquery_top_is_and_or_phrase(tsquery: &str) -> bool {
    if let Ok(tokens) = tokenize_tsquery(tsquery) {
        // Find the top-level binary operator by looking outside any parenthesized groups.
        // Simple approach: scan tokens at nesting depth 0.
        let mut depth = 0i32;
        let mut top_op = None;
        for token in &tokens {
            match token {
                TsQueryToken::LParen => depth += 1,
                TsQueryToken::RParen => depth -= 1,
                TsQueryToken::And | TsQueryToken::FollowedBy(_) if depth == 0 => {
                    top_op = Some(true);
                }
                TsQueryToken::Or if depth == 0 => {
                    // OR at top level means calc_rank_or
                    return false;
                }
                _ => {}
            }
        }
        // If we found AND/PHRASE at top-level, or if there's only terms (single term), use the rule:
        // Single term -> calc_rank_or; multi-term AND/PHRASE -> calc_rank_and
        top_op.unwrap_or(false)
    } else {
        false
    }
}

/// PostgreSQL-compatible word_distance function for calc_rank_and.
/// Returns a proximity score based on position distance between terms.
fn word_distance(dist: i32) -> f64 {
    if dist > 100 {
        return 1e-30;
    }
    1.0 / (1.005 + 0.05 * ((dist as f64) / 1.5 - 2.0).exp())
}

/// PostgreSQL `calc_rank_or` algorithm.
/// For each unique query term: compute weighted inverse-square sum of positions,
/// normalize by π²/6, then average across terms.
fn calc_rank_or(
    word_positions: &HashMap<String, Vec<(u32, char)>>,
    query_terms: &[String],
    weights: &[f64; 4],
) -> f64 {
    const PI2_OVER_6: f64 = 1.644_934_066_85;
    let mut res = 0.0f64;
    for term in query_terms {
        if let Some(pos_weights) = word_positions.get(term.as_str()) {
            if pos_weights.is_empty() {
                continue;
            }
            let mut resj = 0.0f64;
            let mut wjm = -1.0f64; // max weight for this term
            let mut jm = 0usize; // index of max-weight occurrence
            for (j, &(_pos, w)) in pos_weights.iter().enumerate() {
                let wt = weight_value(w, weights);
                resj += wt / ((j as f64 + 1.0) * (j as f64 + 1.0));
                if wt > wjm {
                    wjm = wt;
                    jm = j;
                }
            }
            // PG formula: (wjm + resj - wjm/((jm+1)^2)) / (pi^2/6)
            res += (wjm + resj - wjm / ((jm as f64 + 1.0) * (jm as f64 + 1.0))) / PI2_OVER_6;
        }
    }
    // PG divides by the number of unique query terms (size)
    if query_terms.is_empty() {
        0.0
    } else {
        res / query_terms.len() as f64
    }
}

/// PostgreSQL `calc_rank_and` algorithm.
/// Computes rank based on pairwise proximity of query terms' positions.
fn calc_rank_and(
    word_positions: &HashMap<String, Vec<(u32, char)>>,
    query_terms: &[String],
    weights: &[f64; 4],
) -> f64 {
    // PG falls back to calc_rank_or when fewer than 2 unique query terms
    if query_terms.len() < 2 {
        return calc_rank_or(word_positions, query_terms, weights);
    }
    let mut res = -1.0f64;
    // For each pair of term groups (i, k) where k < i
    // Terms not found in the tsvector are simply skipped (PG: if (!entry) continue)
    for i in 1..query_terms.len() {
        let pos_i = match word_positions.get(query_terms[i].as_str()) {
            Some(pv) if !pv.is_empty() => pv,
            _ => continue,
        };
        for query_term_k in query_terms.iter().take(i) {
            let pos_k = match word_positions.get(query_term_k.as_str()) {
                Some(pv) if !pv.is_empty() => pv,
                _ => continue,
            };
            for &(p_i, w_i) in pos_i {
                for &(p_k, w_k) in pos_k {
                    let dist = (p_i as i32 - p_k as i32).unsigned_abs() as i32;
                    if dist > 0 {
                        let curw = (weight_value(w_i, weights)
                            * weight_value(w_k, weights)
                            * word_distance(dist))
                        .sqrt();
                        if res < 0.0 {
                            res = curw;
                        } else {
                            res = 1.0 - (1.0 - res) * (1.0 - curw);
                        }
                    }
                }
            }
        }
    }
    res
}
fn compute_rank(tsvector: &str, tsquery: &str, norm: u32, weights: &[f64; 4]) -> f64 {
    let word_positions = extract_tsvector_words_with_positions(tsvector);
    let query_terms = extract_query_terms_list(tsquery);
    if query_terms.is_empty() || word_positions.is_empty() {
        return 0.0;
    }
    // PG dispatch: AND or PHRASE at top level → calc_rank_and, else → calc_rank_or
    let mut res = if tsquery_top_is_and_or_phrase(tsquery) {
        calc_rank_and(&word_positions, &query_terms, weights)
    } else {
        calc_rank_or(&word_positions, &query_terms, weights)
    };
    if res < 0.0 {
        res = 1e-20; // PG clamps to 1e-20, not 0 (affects norm bit 32)
    }
    apply_normalization(res, &word_positions, norm)
}
fn compute_rank_cd(tsvector: &str, tsquery: &str, norm: u32, weights: &[f64; 4]) -> f64 {
    let word_positions = extract_tsvector_words_with_positions(tsvector);
    let query_tokens = match tokenize_tsquery(tsquery) {
        Ok(tokens) if !tokens.is_empty() => tokens,
        _ => return 0.0,
    };
    let query_terms = extract_query_terms_list(tsquery);
    if query_terms.is_empty() || word_positions.is_empty() {
        return 0.0;
    }

    let doc = build_rank_cd_doc(&word_positions, &query_terms);
    if doc.is_empty() {
        return 0.0;
    }

    let mut wdoc = 0.0f64;
    let mut sum_dist = 0.0f64;
    let mut prev_ext_pos = 0.0f64;
    let mut n_extent = 0usize;

    let mut start = 0usize;
    while start < doc.len() {
        let Some((begin, end)) = find_next_rank_cd_cover(&query_tokens, &doc, start) else {
            break;
        };

        let mut inv_sum = 0.0f64;
        for (_, weight, _) in &doc[begin..=end] {
            inv_sum += 1.0 / weight_value(*weight, weights);
        }
        if inv_sum > 0.0 {
            let cpos = (end - begin + 1) as f64 / inv_sum;
            let mut n_noise =
                (doc[end].0 as i64 - doc[begin].0 as i64) - (end as i64 - begin as i64);
            if n_noise < 0 {
                n_noise = ((end - begin) / 2) as i64;
            }
            wdoc += cpos / (1.0 + n_noise as f64);
        }

        let cur_ext_pos = (doc[end].0 as f64 + doc[begin].0 as f64) / 2.0;
        if n_extent > 0 && cur_ext_pos > prev_ext_pos {
            sum_dist += 1.0 / (cur_ext_pos - prev_ext_pos);
        }
        prev_ext_pos = cur_ext_pos;
        n_extent += 1;

        start = begin + 1;
    }

    apply_rank_cd_normalization(wdoc, &word_positions, norm, n_extent, sum_dist)
}

fn build_rank_cd_doc(
    word_positions: &HashMap<String, Vec<(u32, char)>>,
    query_terms: &[String],
) -> Vec<(u32, char, String)> {
    let mut doc = Vec::new();
    for term in query_terms {
        if let Some(pos_weights) = word_positions.get(term.as_str()) {
            for &(pos, weight) in pos_weights {
                doc.push((pos, weight, term.clone()));
            }
        }
    }
    doc.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0).then_with(|| lhs.2.cmp(&rhs.2)));
    doc
}

fn rank_cd_window_matches(tokens: &[TsQueryToken], doc_window: &[(u32, char, String)]) -> bool {
    let mut window_positions: HashMap<String, Vec<u32>> = HashMap::new();
    for (pos, _, term) in doc_window {
        window_positions.entry(term.clone()).or_default().push(*pos);
    }
    for positions in window_positions.values_mut() {
        positions.sort_unstable();
        positions.dedup();
    }

    let mut evaluator = TsQueryEvaluator::new(tokens, &window_positions);
    evaluator.eval().unwrap_or(false)
}

fn find_next_rank_cd_cover(
    tokens: &[TsQueryToken],
    doc: &[(u32, char, String)],
    start: usize,
) -> Option<(usize, usize)> {
    let mut end = start;
    while end < doc.len() {
        if rank_cd_window_matches(tokens, &doc[start..=end]) {
            let mut begin = start;
            while begin < end && rank_cd_window_matches(tokens, &doc[begin + 1..=end]) {
                begin += 1;
            }
            return Some((begin, end));
        }
        end += 1;
    }
    None
}

fn apply_rank_cd_normalization(
    score: f64,
    word_positions: &HashMap<String, Vec<(u32, char)>>,
    norm: u32,
    n_extent: usize,
    sum_dist: f64,
) -> f64 {
    if norm == 0 {
        return score;
    }

    let cnt_length: usize = word_positions.values().map(|v| v.len()).sum();
    let unique_words = word_positions.len();

    let mut result = score;
    if norm & 1 != 0 && cnt_length > 0 {
        result /= ((cnt_length + 1) as f64).ln();
    }
    if norm & 2 != 0 && cnt_length > 0 {
        result /= cnt_length as f64;
    }
    if norm & 4 != 0 && n_extent > 0 && sum_dist > 0.0 {
        result /= n_extent as f64 / sum_dist;
    }
    if norm & 8 != 0 && unique_words > 0 {
        result /= unique_words as f64;
    }
    if norm & 16 != 0 && unique_words > 0 {
        result /= ((unique_words + 1) as f64).ln() / (2.0f64).ln();
    }
    if norm & 32 != 0 {
        result = result / (result + 1.0);
    }
    result
}
fn extract_query_terms_list(tsquery: &str) -> Vec<String> {
    if let Ok(tokens) = tokenize_tsquery(tsquery) {
        // Deduplicate terms (PG's SortAndUniqItems)
        let mut seen = std::collections::HashSet::new();
        tokens
            .into_iter()
            .filter_map(|t| match t {
                TsQueryToken::Term(s) => {
                    let lower = s.to_lowercase();
                    if seen.insert(lower.clone()) {
                        Some(lower)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    }
}
fn apply_normalization(
    score: f64,
    word_positions: &HashMap<String, Vec<(u32, char)>>,
    norm: u32,
) -> f64 {
    if norm == 0 {
        return score;
    }
    // cnt_length: total number of lexeme positions in the tsvector
    let cnt_length: usize = word_positions.values().map(|v| v.len()).sum();
    // t->size: number of unique lexemes
    let unique_words = word_positions.len();
    let mut result = score;
    // RANK_NORM_LOGLENGTH (1): divide by log2(cnt_length + 1)
    if norm & 1 != 0 && cnt_length > 0 {
        result /= ((cnt_length + 1) as f64).ln() / (2.0f64).ln();
    }
    // RANK_NORM_LENGTH (2): divide by cnt_length
    if norm & 2 != 0 && cnt_length > 0 {
        result /= cnt_length as f64;
    }
    // RANK_NORM_EXTDIST (4): not applicable per PG
    // RANK_NORM_UNIQ (8): divide by number of unique lexemes
    if norm & 8 != 0 && unique_words > 0 {
        result /= unique_words as f64;
    }
    // RANK_NORM_LOGUNIQ (16): divide by log2(unique_words + 1)
    if norm & 16 != 0 && unique_words > 0 {
        result /= ((unique_words + 1) as f64).ln() / (2.0f64).ln();
    }
    // RANK_NORM_RDIVRPLUS1 (32): rank / (rank + 1)
    if norm & 32 != 0 {
        result = result / (result + 1.0);
    }
    result
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
            websearch_to_tsquery(vec![Value::Text("simple".into()), Value::Text("".into())])
                .unwrap();
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
}
