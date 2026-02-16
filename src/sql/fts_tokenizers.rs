//! Tokenizer registry for full-text search.
//!
//! This module provides a pluggable tokenizer system for multiple languages,
//! supporting both simple English tokenization and Chinese tokenization via jieba-rs.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Tokenizer function type: takes text and returns a vector of tokens.
pub type TokenizerFn = fn(&str) -> Vec<String>;

/// Global tokenizer registry, initialized once on first access.
static TOKENIZER_REGISTRY: OnceLock<HashMap<String, TokenizerFn>> = OnceLock::new();

/// Get a tokenizer by configuration name.
///
/// Supported configurations:
/// - "simple", "english": Simple alphanumeric tokenization (suitable for English)
/// - "chinese", "jieba": Chinese word segmentation using jieba-rs
///
/// Returns `None` if the configuration name is not registered.
pub fn get_tokenizer(config: &str) -> Option<TokenizerFn> {
    TOKENIZER_REGISTRY
        .get_or_init(init_tokenizers)
        .get(config)
        .copied()
}

/// Initialize the tokenizer registry with all available tokenizers.
fn init_tokenizers() -> HashMap<String, TokenizerFn> {
    let mut map = HashMap::with_capacity(4);
    map.insert("simple".to_string(), tokenize_simple as TokenizerFn);
    map.insert("english".to_string(), tokenize_simple as TokenizerFn);
    map.insert("chinese".to_string(), tokenize_jieba as TokenizerFn);
    map.insert("jieba".to_string(), tokenize_jieba as TokenizerFn);
    map
}

/// Simple tokenizer for English and alphanumeric text.
///
/// Splits on non-alphanumeric characters and filters out:
/// - Empty strings
/// - Single-character tokens
///
/// All tokens are converted to lowercase.
fn tokenize_simple(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() > 1)
        .map(|s| s.to_string())
        .collect()
}

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
    let jieba = JIEBA.get_or_init(|| Jieba::new());

    // Cut text into words (no HMM for better performance)
    jieba
        .cut(text, false)
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Get the default text search configuration.
///
/// Can be overridden via the `TIPG_DEFAULT_TEXT_SEARCH_CONFIG` environment variable.
/// Defaults to "simple" if not set.
pub fn default_text_search_config() -> &'static str {
    static DEFAULT: OnceLock<String> = OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            std::env::var("TIPG_DEFAULT_TEXT_SEARCH_CONFIG")
                .unwrap_or_else(|_| "simple".to_string())
        })
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_english_tokenizer() {
        let tokens = tokenize_simple("Hello, World! This is a test.");
        assert_eq!(tokens, vec!["hello", "world", "this", "is", "test"]);
    }

    #[test]
    fn test_english_tokenizer_filters_single_chars() {
        let tokens = tokenize_simple("I am a student");
        // "I", "a" are filtered out (single chars)
        assert_eq!(tokens, vec!["am", "student"]);
    }

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

    #[test]
    fn test_tokenizer_registry() {
        assert!(get_tokenizer("english").is_some());
        assert!(get_tokenizer("simple").is_some());
        assert!(get_tokenizer("chinese").is_some());
        assert!(get_tokenizer("jieba").is_some());
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
    }
}
