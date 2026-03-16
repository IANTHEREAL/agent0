//! Break point detection and scoring for markdown documents.

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub(crate) struct BreakPoint {
    pub(crate) pos: usize,
    pub(crate) score: u32,
}

// Heading scores by level (index 0 unused, 1=H1..6=H6)
const HEADING_SCORES: [u32; 7] = [0, 100, 90, 80, 70, 60, 50];

struct SimplePattern {
    regex: Regex,
    score: u32,
}

static SIMPLE_PATTERNS: LazyLock<Vec<SimplePattern>> = LazyLock::new(|| {
    vec![
        SimplePattern {
            regex: Regex::new(r"\n```").unwrap(),
            score: 80,
        }, // code fence
        SimplePattern {
            regex: Regex::new(r"\n(?:---|\*\*\*|___)\s*\n").unwrap(),
            score: 60,
        }, // hr
        SimplePattern {
            regex: Regex::new(r"\n\n+").unwrap(),
            score: 20,
        }, // blank line
        SimplePattern {
            regex: Regex::new(r"\n[-*]\s").unwrap(),
            score: 5,
        }, // list item
        SimplePattern {
            regex: Regex::new(r"\n\d+\.\s").unwrap(),
            score: 5,
        }, // numbered list
        SimplePattern {
            regex: Regex::new(r"\n").unwrap(),
            score: 1,
        }, // newline
    ]
});

// Matches \n followed by 1-6 # chars then a space (markdown heading).
static HEADING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n(#{1,6})\s").unwrap());

/// Scan text for all potential break points, keeping only the highest-scoring
/// match at each position.
pub(crate) fn scan_break_points(text: &str) -> Vec<BreakPoint> {
    let mut seen: BTreeMap<usize, u32> = BTreeMap::new();

    // Detect headings: \n#{1,6}\s — score depends on heading level
    for cap in HEADING_RE.captures_iter(text) {
        let m = cap.get(0).unwrap();
        let pos = m.start();
        let level = cap[1].len(); // number of # chars
        let score = HEADING_SCORES.get(level).copied().unwrap_or(50);
        let entry = seen.entry(pos).or_insert(0);
        if score > *entry {
            *entry = score;
        }
    }

    // Other patterns
    for pattern in SIMPLE_PATTERNS.iter() {
        for m in pattern.regex.find_iter(text) {
            let pos = m.start();
            let entry = seen.entry(pos).or_insert(0);
            if pattern.score > *entry {
                *entry = pattern.score;
            }
        }
    }

    seen.into_iter()
        .map(|(pos, score)| BreakPoint { pos, score })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_heading_break_points() {
        let text = "intro\n# Heading 1\n## Heading 2\nmore text";
        let bps = scan_break_points(text);
        let h1 = bps.iter().find(|bp| bp.score == 100);
        assert!(h1.is_some(), "should detect H1");
        let h2 = bps.iter().find(|bp| bp.score == 90);
        assert!(h2.is_some(), "should detect H2");
    }

    #[test]
    fn highest_score_wins_at_same_position() {
        let text = "hello\n## heading";
        let bps = scan_break_points(text);
        // The \n at position 5 matches both newline (1) and h2 (90)
        let bp_at_5 = bps.iter().find(|bp| bp.pos == 5).unwrap();
        assert_eq!(bp_at_5.score, 90);
    }

    #[test]
    fn detects_blank_lines() {
        let text = "first\n\nsecond";
        let bps = scan_break_points(text);
        let blank = bps.iter().find(|bp| bp.score == 20);
        assert!(blank.is_some(), "should detect blank line");
    }

    #[test]
    fn h3_through_h6_detected() {
        let text = "a\n### H3\n#### H4\n##### H5\n###### H6\n";
        let bps = scan_break_points(text);
        assert!(bps.iter().any(|bp| bp.score == 80), "H3");
        assert!(bps.iter().any(|bp| bp.score == 70), "H4");
        assert!(bps.iter().any(|bp| bp.score == 60), "H5");
        assert!(bps.iter().any(|bp| bp.score == 50), "H6");
    }
}
