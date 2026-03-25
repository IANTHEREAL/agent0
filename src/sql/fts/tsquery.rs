use super::*;

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

pub(crate) fn parse_norm(val: &Value, func: &str) -> Result<u32> {
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

pub(crate) fn parse_weights(val: &Value, func: &str) -> Result<[f64; 4]> {
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
pub(crate) fn extract_tsvector_words_with_positions(
    tsvector: &str,
) -> HashMap<String, Vec<(u32, char)>> {
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
pub(crate) fn extract_positions_only(tsvector: &str) -> HashMap<String, Vec<u32>> {
    extract_tsvector_words_with_positions(tsvector)
        .into_iter()
        .map(|(word, pws)| (word, pws.into_iter().map(|(p, _)| p).collect()))
        .collect()
}

/// Parse a position list like `1A,3B,5` into `[(1,'A'), (3,'B'), (5,'D')]`.
pub(crate) fn parse_position_list(s: &str) -> Vec<(u32, char)> {
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
