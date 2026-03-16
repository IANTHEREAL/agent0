//! Code fence region detection.

use std::ops::Range;

/// Find all code fence regions (``` ... ```) in the text.
/// An unclosed fence extends to the end of the document.
pub(crate) fn find_code_fences(text: &str) -> Vec<Range<usize>> {
    let mut regions = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0;

    // Match \n``` (fence markers must start at a newline)
    let mut search_start = 0;
    while let Some(offset) = text[search_start..].find("\n```") {
        let pos = search_start + offset;
        if !in_fence {
            fence_start = pos;
            in_fence = true;
        } else {
            regions.push(fence_start..pos + 4); // include the closing \n```
            in_fence = false;
        }
        search_start = pos + 4; // skip past \n```
        if search_start >= text.len() {
            break;
        }
    }

    // Unclosed fence extends to end of document
    if in_fence {
        regions.push(fence_start..text.len());
    }

    regions
}

/// Check if a position falls inside any code fence region.
pub(crate) fn is_inside_code_fence(pos: usize, fences: &[Range<usize>]) -> bool {
    fences.iter().any(|f| pos > f.start && pos < f.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_paired_fences() {
        let text = "before\n```\ncode\n```\nafter";
        let fences = find_code_fences(text);
        assert_eq!(fences.len(), 1);
        assert!(fences[0].start < fences[0].end);
    }

    #[test]
    fn unclosed_fence_extends_to_end() {
        let text = "before\n```\ncode without closing";
        let fences = find_code_fences(text);
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].end, text.len());
    }

    #[test]
    fn position_inside_fence() {
        let text = "before\n```\ncode\n```\nafter";
        let fences = find_code_fences(text);
        // Position inside the code block
        let code_pos = text.find("code").unwrap();
        assert!(is_inside_code_fence(code_pos, &fences));
        // Position outside
        let after_pos = text.find("after").unwrap();
        assert!(!is_inside_code_fence(after_pos, &fences));
    }

    #[test]
    fn no_fences_in_plain_text() {
        let text = "just plain text\nwith newlines\n";
        let fences = find_code_fences(text);
        assert!(fences.is_empty());
    }
}
