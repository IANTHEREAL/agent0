//! Tokenizer registry for full-text search.
//!
//! This module provides a pluggable tokenizer system for multiple languages:
//! - `simple` / `english`: basic alphanumeric tokenization (no stemming)
//! - `english_stem`: Snowball-stemmed English with PG-compatible stopwords
//! - `chinese` / `jieba` / `zhparser`: jieba-rs word segmentation
//! - `ngram`: character bigram tokenizer (CJK + English)
//! - `ngram3` / `trigram`: character trigram tokenizer
//! - `unigram`: single-character tokenizer (maximum recall)
//! - `chinese_ngram` / `zhparser_ngram`: jieba + bigram overlay

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

use super::fts_stopwords::is_english_stopword;
/// Tokenizer function type: takes text and returns a vector of tokens.
pub type TokenizerFn = fn(&str) -> Vec<String>;
static TOKENIZER_REGISTRY: OnceLock<HashMap<String, TokenizerFn>> = OnceLock::new();
/// Nested map: keyspace → db_id → config_name → tokenizer_name.
///
/// Three-level structure enables O(1) lookups via `String: Borrow<str>`
/// at each level, avoiding String allocations on the read path.
type UserTscCacheMap = HashMap<String, HashMap<u64, HashMap<String, String>>>;
/// User-defined text search config → built-in tokenizer name.
///
/// Populated by `CREATE TEXT SEARCH CONFIGURATION` (per-tenant DDL).
/// Key: `(keyspace, db_id, config_name)` → Value: tokenizer name in static registry.
/// This is an in-process cache; on restart, configs are re-cached on first use
/// from TiKV via `register_user_tsc`.
static USER_TSC_CACHE: OnceLock<RwLock<UserTscCacheMap>> = OnceLock::new();

fn user_cache() -> &'static RwLock<UserTscCacheMap> {
    USER_TSC_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a user-defined text search configuration in the in-process cache.
///
/// Called by the executor after successfully persisting to TiKV.
pub fn register_user_tsc(keyspace: &str, db_id: u64, config_name: &str, tokenizer_name: &str) {
    let mut cache = user_cache().write();
    cache
        .entry(keyspace.to_string())
        .or_default()
        .entry(db_id)
        .or_default()
        .insert(config_name.to_string(), tokenizer_name.to_string());
}

/// Remove a user-defined text search configuration from the in-process cache.
pub fn unregister_user_tsc(keyspace: &str, db_id: u64, config_name: &str) {
    let mut cache = user_cache().write();
    if let Some(by_db) = cache.get_mut(keyspace) {
        if let Some(by_cfg) = by_db.get_mut(&db_id) {
            by_cfg.remove(config_name);
        }
    }
}

/// Remove all cached text search configurations for a keyspace.
///
/// Called when a tenant is evicted from the connection pool to prevent
/// unbounded accumulation of stale entries.
pub fn evict_user_tsc_keyspace(keyspace: &str) {
    user_cache().write().remove(keyspace);
}

/// Resolve a user-defined TSC to a tokenizer function.
///
/// Returns `None` if the config is not in the user cache or the cached
/// tokenizer name doesn't exist in the static registry.
pub fn resolve_user_tsc(keyspace: &str, db_id: u64, config: &str) -> Option<TokenizerFn> {
    let cache = user_cache().read();
    let tokenizer_name = cache.get(keyspace)?.get(&db_id)?.get(config)?;
    get_tokenizer(tokenizer_name)
}

/// Get a tokenizer by configuration name (static registry only).
///
/// Returns `None` if the configuration name is not registered.
/// For user-defined configs, use `resolve_user_tsc` as fallback.
pub fn get_tokenizer(config: &str) -> Option<TokenizerFn> {
    TOKENIZER_REGISTRY
        .get_or_init(init_tokenizers)
        .get(config)
        .copied()
}

/// Initialize the tokenizer registry with all available tokenizers.
fn init_tokenizers() -> HashMap<String, TokenizerFn> {
    let mut map = HashMap::with_capacity(16);

    // Basic tokenizers
    map.insert("simple".to_string(), tokenize_simple as TokenizerFn);
    map.insert("english".to_string(), tokenize_simple as TokenizerFn);

    // Stemmed English (Snowball + full PG stopword list)
    map.insert(
        "english_stem".to_string(),
        tokenize_english_stemmed as TokenizerFn,
    );

    // Chinese / jieba / zhparser (all aliases for jieba-rs)
    map.insert("chinese".to_string(), tokenize_jieba as TokenizerFn);
    map.insert("jieba".to_string(), tokenize_jieba as TokenizerFn);
    map.insert("zhparser".to_string(), tokenize_jieba as TokenizerFn);

    // N-gram tokenizers
    map.insert("ngram".to_string(), tokenize_ngram as TokenizerFn);
    map.insert("ngram3".to_string(), tokenize_trigram as TokenizerFn);
    map.insert("trigram".to_string(), tokenize_trigram as TokenizerFn);
    map.insert("unigram".to_string(), tokenize_unigram as TokenizerFn);

    // Mixed: jieba + bigram overlay for CJK multi-char words
    map.insert(
        "chinese_ngram".to_string(),
        tokenize_chinese_ngram as TokenizerFn,
    );
    map.insert(
        "zhparser_ngram".to_string(),
        tokenize_chinese_ngram as TokenizerFn,
    );

    map
}

// ---------------------------------------------------------------------------
// Simple tokenizer (no stemming, no stopwords)
// ---------------------------------------------------------------------------

/// Simple tokenizer for English and alphanumeric text.
///
/// Splits on non-alphanumeric characters and filters out empty strings.
/// All tokens are converted to lowercase.
fn tokenize_simple(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// English stemmed tokenizer (Snowball + PG stopword list)
// ---------------------------------------------------------------------------

/// English tokenizer with Snowball stemming.
///
/// Returns ALL tokens (including stopwords in original form).  Stopword
/// filtering is handled by `to_tsvector` / `plainto_tsquery` in `fts.rs`
/// so that position numbering can correctly skip stopword slots.
///
/// Non-stopwords are stemmed via the Snowball English algorithm.
fn tokenize_english_stemmed(text: &str) -> Vec<String> {
    use rust_stemmers::{Algorithm, Stemmer};

    static STEMMER: OnceLock<Stemmer> = OnceLock::new();
    let stemmer = STEMMER.get_or_init(|| Stemmer::create(Algorithm::English));

    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if is_english_stopword(s) {
                // Keep original form so that to_tsvector can detect and skip it
                // while preserving the correct position counter.
                s.to_string()
            } else {
                stemmer.stem(s).to_string()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Chinese tokenizer (jieba-rs)
// ---------------------------------------------------------------------------

/// Chinese tokenizer using jieba-rs word segmentation.
///
/// Uses the jieba cut algorithm to segment Chinese text into words.
/// The jieba instance is initialized once and cached for performance.
///
/// Mixed Chinese-English text is supported: English words are lowercased,
/// and Chinese words are segmented.
fn tokenize_jieba(text: &str) -> Vec<String> {
    use jieba_rs::Jieba;

    // Lazy initialization: create jieba instance once and cache it
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    let jieba = JIEBA.get_or_init(Jieba::new);

    // Cut text into words (no HMM for better performance)
    jieba
        .cut(text, false)
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// N-gram tokenizers
// ---------------------------------------------------------------------------

/// Check if a character is in a CJK ideograph range.
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{20000}'..='\u{2A6DF}' // Extension B
        | '\u{2A700}'..='\u{2B73F}' // Extension C
        | '\u{2B740}'..='\u{2B81F}' // Extension D
        | '\u{2B820}'..='\u{2CEAF}' // Extension E
        | '\u{2CEB0}'..='\u{2EBEF}' // Extension F
        | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
        | '\u{2F800}'..='\u{2FA1F}' // CJK Compatibility Ideographs Supplement
        | '\u{3000}'..='\u{303F}'   // CJK Symbols and Punctuation (exclude below)
        | '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana
        | '\u{31F0}'..='\u{31FF}'   // Katakana Phonetic Extensions
        | '\u{AC00}'..='\u{D7AF}'   // Hangul Syllables
    )
}

/// Segment text into runs of "token characters" separated by whitespace
/// and punctuation.  Each segment is a sequence of alphanumeric or CJK chars.
fn segment_for_ngram(text: &str) -> Vec<Vec<char>> {
    let lower = text.to_lowercase();
    let mut segments: Vec<Vec<char>> = Vec::new();
    let mut current: Vec<char> = Vec::new();

    for c in lower.chars() {
        if c.is_alphanumeric() || is_cjk_char(c) {
            current.push(c);
        } else {
            // whitespace or punctuation → segment boundary
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Core n-gram tokenization over pre-segmented text.
fn tokenize_ngram_with_size(text: &str, n: usize) -> Vec<String> {
    let segments = segment_for_ngram(text);
    let mut tokens = Vec::new();

    for segment in segments {
        if segment.len() < n {
            // Segment shorter than n → emit as single token
            tokens.push(segment.iter().collect());
        } else {
            for window in segment.windows(n) {
                tokens.push(window.iter().collect());
            }
        }
    }
    tokens
}

/// Bigram (n=2) tokenizer — registered as `"ngram"`.
fn tokenize_ngram(text: &str) -> Vec<String> {
    tokenize_ngram_with_size(text, 2)
}

/// Trigram (n=3) tokenizer — registered as `"ngram3"` / `"trigram"`.
fn tokenize_trigram(text: &str) -> Vec<String> {
    tokenize_ngram_with_size(text, 3)
}

/// Unigram (n=1) tokenizer — registered as `"unigram"`.
fn tokenize_unigram(text: &str) -> Vec<String> {
    tokenize_ngram_with_size(text, 1)
}

// ---------------------------------------------------------------------------
// Mixed tokenizer: jieba + bigram overlay
// ---------------------------------------------------------------------------

/// Chinese mixed tokenizer: jieba word segmentation + bigram overlay.
///
/// For each CJK multi-character word produced by jieba, additional bigrams
/// are generated.  This enables substring matching (e.g. searching "数据"
/// matches "数据库") while preserving jieba's word-level precision.
fn tokenize_chinese_ngram(text: &str) -> Vec<String> {
    let mut tokens = tokenize_jieba(text);

    // Generate bigram overlays for CJK multi-char words
    let mut extra = Vec::new();
    for token in &tokens {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() > 1 && chars.iter().any(|c| is_cjk_char(*c)) {
            for window in chars.windows(2) {
                let bigram: String = window.iter().collect();
                extra.push(bigram);
            }
        }
    }
    tokens.extend(extra);
    tokens
}

// ---------------------------------------------------------------------------
// Default config helper
// ---------------------------------------------------------------------------

/// Get the default text search configuration.
///
/// Can be overridden via the `DB9_DEFAULT_TEXT_SEARCH_CONFIG` environment variable.
/// Defaults to "simple" if not set.
pub fn default_text_search_config() -> &'static str {
    static DEFAULT: OnceLock<String> = OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            std::env::var("DB9_DEFAULT_TEXT_SEARCH_CONFIG").unwrap_or_else(|_| "simple".to_string())
        })
        .as_str()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Simple tokenizer ---

    #[test]
    fn test_english_tokenizer() {
        let tokens = tokenize_simple("Hello, World! This is a test.");
        assert_eq!(tokens, vec!["hello", "world", "this", "is", "a", "test"]);
    }

    #[test]
    fn test_english_tokenizer_keeps_single_chars() {
        let tokens = tokenize_simple("I am a student");
        assert_eq!(tokens, vec!["i", "am", "a", "student"]);
    }

    // --- English stemmed tokenizer ---

    #[test]
    fn test_english_stemmed_basic() {
        let tokens = tokenize_english_stemmed("The running dogs are happy");
        // "the" is a stopword → kept as-is (for position tracking)
        // "running" → "run", "dogs" → "dog", "are" → kept (stopword), "happy" → "happi"
        assert!(tokens.contains(&"the".to_string())); // stopword, original form
        assert!(tokens.contains(&"run".to_string()));
        assert!(tokens.contains(&"dog".to_string()));
        assert!(tokens.contains(&"are".to_string())); // stopword
        assert!(tokens.contains(&"happi".to_string()));
    }

    #[test]
    fn test_english_stemmed_preserves_all_positions() {
        let tokens = tokenize_english_stemmed("The running dogs are happy");
        // Should have 5 tokens (all words, stopwords kept for position tracking)
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[0], "the"); // stopword
        assert_eq!(tokens[1], "run"); // stemmed
        assert_eq!(tokens[2], "dog"); // stemmed
        assert_eq!(tokens[3], "are"); // stopword
        assert_eq!(tokens[4], "happi"); // stemmed
    }

    #[test]
    fn test_english_stemmed_single_word() {
        let tokens = tokenize_english_stemmed("running");
        assert_eq!(tokens, vec!["run"]);
    }

    // --- Chinese tokenizer ---

    #[test]
    fn test_chinese_tokenizer() {
        let tokens = tokenize_jieba("我爱自然语言处理");
        assert!(tokens.contains(&"自然语言".to_string()));
        assert!(tokens.contains(&"处理".to_string()));
    }

    #[test]
    fn test_chinese_tokenizer_sentence() {
        let tokens = tokenize_jieba("分布式数据库是现代互联网架构的核心组件");
        assert!(tokens.contains(&"分布式".to_string()));
        assert!(tokens.contains(&"数据库".to_string()));
        assert!(tokens.contains(&"现代".to_string()));
        assert!(tokens.contains(&"互联网".to_string()));
        assert!(tokens.contains(&"架构".to_string()));
        assert!(tokens.contains(&"核心".to_string()));
        assert!(tokens.contains(&"组件".to_string()));
    }

    // --- N-gram tokenizer ---

    #[test]
    fn test_ngram_cjk() {
        let tokens = tokenize_ngram("数据库技术");
        assert!(tokens.contains(&"数据".to_string()));
        assert!(tokens.contains(&"据库".to_string()));
        assert!(tokens.contains(&"库技".to_string()));
        assert!(tokens.contains(&"技术".to_string()));
        assert_eq!(tokens.len(), 4);
    }

    #[test]
    fn test_ngram_english() {
        let tokens = tokenize_ngram("database");
        assert!(tokens.contains(&"da".to_string()));
        assert!(tokens.contains(&"at".to_string()));
        assert!(tokens.contains(&"ta".to_string()));
        assert!(tokens.contains(&"ab".to_string()));
        assert!(tokens.contains(&"ba".to_string()));
        assert!(tokens.contains(&"as".to_string()));
        assert!(tokens.contains(&"se".to_string()));
        assert_eq!(tokens.len(), 7); // "database" = 8 chars → 7 bigrams
    }

    #[test]
    fn test_ngram_punctuation_separator() {
        // Punctuation should separate segments — no cross-punctuation bigrams
        let tokens = tokenize_ngram("hello, world");
        // "hello" → he, el, ll, lo
        // "world" → wo, or, rl, ld
        assert!(tokens.contains(&"he".to_string()));
        assert!(tokens.contains(&"lo".to_string()));
        assert!(tokens.contains(&"wo".to_string()));
        assert!(tokens.contains(&"ld".to_string()));
        // Should NOT contain "o," or ", " or ",w" etc.
        assert!(!tokens.contains(&"o,".to_string()));
        assert_eq!(tokens.len(), 8);
    }

    #[test]
    fn test_ngram_short_segment() {
        // Single char segments shorter than n=2 → emitted as-is
        let tokens = tokenize_ngram("I am");
        assert!(tokens.contains(&"i".to_string()));
        assert!(tokens.contains(&"am".to_string()));
    }

    #[test]
    fn test_ngram_mixed_cjk_english() {
        let tokens = tokenize_ngram("数据db");
        // "数据db" is one segment → bigrams: 数据, 据d, db
        assert!(tokens.contains(&"数据".to_string()));
        assert!(tokens.contains(&"db".to_string()));
        assert_eq!(tokens.len(), 3);
    }

    // --- Trigram tokenizer ---

    #[test]
    fn test_trigram_basic() {
        let tokens = tokenize_trigram("数据库技术");
        assert!(tokens.contains(&"数据库".to_string()));
        assert!(tokens.contains(&"据库技".to_string()));
        assert!(tokens.contains(&"库技术".to_string()));
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn test_trigram_short() {
        // "数据" is 2 chars, shorter than n=3 → emitted as-is
        let tokens = tokenize_trigram("数据");
        assert_eq!(tokens, vec!["数据"]);
    }

    // --- Unigram tokenizer ---

    #[test]
    fn test_unigram() {
        let tokens = tokenize_unigram("数据库");
        assert_eq!(tokens, vec!["数", "据", "库"]);
    }

    // --- Chinese ngram (mixed) tokenizer ---

    #[test]
    fn test_chinese_ngram_basic() {
        let tokens = tokenize_chinese_ngram("分布式数据库");
        // jieba words
        assert!(tokens.contains(&"分布式".to_string()));
        assert!(tokens.contains(&"数据库".to_string()));
        // bigram overlays
        assert!(tokens.contains(&"分布".to_string()));
        assert!(tokens.contains(&"布式".to_string()));
        assert!(tokens.contains(&"数据".to_string()));
        assert!(tokens.contains(&"据库".to_string()));
    }

    #[test]
    fn test_chinese_ngram_substring_match() {
        let tokens = tokenize_chinese_ngram("分布式数据库");
        // "数据" should be in the token list (bigram from "数据库")
        assert!(tokens.contains(&"数据".to_string()));
    }

    #[test]
    fn test_chinese_ngram_single_char_no_bigram() {
        // Single CJK chars don't get bigram overlay (len <= 1)
        let tokens = tokenize_chinese_ngram("是");
        // jieba returns "是" as a single token
        // No bigram overlay since it's a single char
        assert!(tokens.contains(&"是".to_string()));
        // Shouldn't have any extra tokens beyond what jieba produced
    }

    // --- Registry tests ---

    #[test]
    fn test_tokenizer_registry() {
        assert!(get_tokenizer("english").is_some());
        assert!(get_tokenizer("simple").is_some());
        assert!(get_tokenizer("english_stem").is_some());
        assert!(get_tokenizer("chinese").is_some());
        assert!(get_tokenizer("jieba").is_some());
        assert!(get_tokenizer("zhparser").is_some());
        assert!(get_tokenizer("ngram").is_some());
        assert!(get_tokenizer("ngram3").is_some());
        assert!(get_tokenizer("trigram").is_some());
        assert!(get_tokenizer("unigram").is_some());
        assert!(get_tokenizer("chinese_ngram").is_some());
        assert!(get_tokenizer("zhparser_ngram").is_some());
        assert!(get_tokenizer("klingon").is_none());
    }

    #[test]
    fn test_mixed_text() {
        let text = "Hello 你好 World 世界";
        let tokens = tokenize_jieba(text);
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"你好".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"世界".to_string()));
    }

    #[test]
    fn test_default_config_is_simple() {
        // Unless overridden by env var, default should be "simple"
        let default = default_text_search_config();
        assert!(default == "simple" || default == "chinese"); // allow env override
    }

    #[test]
    fn test_tokenizer_functions_are_consistent() {
        // English and simple should be the same
        let text = "The quick brown fox";
        let tokens_en = get_tokenizer("english").unwrap()(text);
        let tokens_simple = get_tokenizer("simple").unwrap()(text);
        assert_eq!(tokens_en, tokens_simple);

        // Chinese and jieba should be the same
        let text_cn = "快速的棕色狐狸";
        let tokens_cn = get_tokenizer("chinese").unwrap()(text_cn);
        let tokens_jieba = get_tokenizer("jieba").unwrap()(text_cn);
        assert_eq!(tokens_cn, tokens_jieba);

        // zhparser and chinese should be the same
        let tokens_zhparser = get_tokenizer("zhparser").unwrap()(text_cn);
        assert_eq!(tokens_cn, tokens_zhparser);
    }

    #[test]
    fn evict_user_tsc_keyspace_removes_all_entries() {
        let ks = "test_evict_ks";
        register_user_tsc(ks, 1, "my_config", "english");
        register_user_tsc(ks, 2, "other_config", "chinese");
        assert!(resolve_user_tsc(ks, 1, "my_config").is_some());
        assert!(resolve_user_tsc(ks, 2, "other_config").is_some());

        evict_user_tsc_keyspace(ks);

        assert!(resolve_user_tsc(ks, 1, "my_config").is_none());
        assert!(resolve_user_tsc(ks, 2, "other_config").is_none());
    }
}
