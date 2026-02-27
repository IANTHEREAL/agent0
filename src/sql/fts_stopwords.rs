//! English stopword list for full-text search.
//!
//! Embeds the PostgreSQL 17 / Snowball English stopword list (~174 words).
//! Used by the `english_stem` text search configuration to remove common
//! function words before stemming.

use std::collections::HashSet;
use std::sync::OnceLock;

/// PostgreSQL-compatible English stopword list (Snowball project).
///
/// Source: PostgreSQL `tsearch_data/english.stop` — the standard list shipped
/// with the Snowball-based `english` text search configuration.
const ENGLISH_STOPWORDS: &[&str] = &[
    "i",
    "me",
    "my",
    "myself",
    "we",
    "our",
    "ours",
    "ourselves",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "what",
    "which",
    "who",
    "whom",
    "this",
    "that",
    "these",
    "those",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "having",
    "do",
    "does",
    "did",
    "doing",
    "would",
    "should",
    "could",
    "ought",
    "i'm",
    "you're",
    "he's",
    "she's",
    "it's",
    "we're",
    "they're",
    "i've",
    "you've",
    "we've",
    "they've",
    "i'd",
    "you'd",
    "he'd",
    "she'd",
    "we'd",
    "they'd",
    "i'll",
    "you'll",
    "he'll",
    "she'll",
    "we'll",
    "they'll",
    "isn't",
    "aren't",
    "wasn't",
    "weren't",
    "hasn't",
    "haven't",
    "hadn't",
    "doesn't",
    "don't",
    "didn't",
    "won't",
    "wouldn't",
    "shan't",
    "shouldn't",
    "can't",
    "cannot",
    "couldn't",
    "mustn't",
    "let's",
    "that's",
    "who's",
    "what's",
    "here's",
    "there's",
    "when's",
    "where's",
    "why's",
    "how's",
    "a",
    "an",
    "the",
    "and",
    "but",
    "if",
    "or",
    "because",
    "as",
    "until",
    "while",
    "of",
    "at",
    "by",
    "for",
    "with",
    "about",
    "against",
    "between",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "all",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "s",
    "t",
    "can",
    "will",
    "just",
    "don",
    "should",
    "now",
    "d",
    "ll",
    "m",
    "o",
    "re",
    "ve",
    "y",
    "ain",
    "aren",
    "couldn",
    "didn",
    "doesn",
    "hadn",
    "hasn",
    "haven",
    "isn",
    "ma",
    "mightn",
    "mustn",
    "needn",
    "shan",
    "shouldn",
    "wasn",
    "weren",
    "won",
    "wouldn",
];

/// Global stopword set, initialized once on first access.
static STOPWORD_SET: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Check whether a word is an English stopword (PG-compatible list).
pub fn is_english_stopword(word: &str) -> bool {
    STOPWORD_SET
        .get_or_init(|| ENGLISH_STOPWORDS.iter().copied().collect())
        .contains(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_stopwords() {
        assert!(is_english_stopword("the"));
        assert!(is_english_stopword("a"));
        assert!(is_english_stopword("is"));
        assert!(is_english_stopword("an"));
        assert!(is_english_stopword("and"));
        assert!(is_english_stopword("or"));
        assert!(is_english_stopword("not"));
    }

    #[test]
    fn test_contraction_fragments() {
        // PG Snowball list includes fragments from tokenized contractions
        assert!(is_english_stopword("don"));
        assert!(is_english_stopword("ll"));
        assert!(is_english_stopword("ve"));
        assert!(is_english_stopword("wouldn"));
    }

    #[test]
    fn test_non_stopwords() {
        assert!(!is_english_stopword("database"));
        assert!(!is_english_stopword("query"));
        assert!(!is_english_stopword("run"));
        assert!(!is_english_stopword("happy"));
    }

    #[test]
    fn test_stopword_count() {
        let set = STOPWORD_SET.get_or_init(|| ENGLISH_STOPWORDS.iter().copied().collect());
        // Full Snowball list: ~204 unique entries (includes contraction fragments)
        assert!(set.len() >= 190);
        assert!(set.len() <= 220);
    }
}
