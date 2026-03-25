use super::*;

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
