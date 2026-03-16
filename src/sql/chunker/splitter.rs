//! Core document chunking algorithm.

use super::break_points::{scan_break_points, BreakPoint};
use super::code_fences::{find_code_fences, is_inside_code_fence};
use std::ops::Range;

/// Maximum input size (1 MB).
const MAX_INPUT_CHARS: usize = 1_048_576;

/// Default chunk size in characters (~900 tokens).
pub const DEFAULT_MAX_CHARS: usize = 3600;

/// Default overlap in characters (15% of chunk size).
pub const DEFAULT_OVERLAP_CHARS: usize = 540;

/// Default search window for finding break points.
const DEFAULT_WINDOW_CHARS: usize = 800;

/// Decay factor for distance-based score weighting.
const DECAY_FACTOR: f64 = 0.7;

#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub(crate) text: String,
    pub(crate) pos: usize,
    pub(crate) index: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkOptions {
    pub(crate) max_chars: usize,
    pub(crate) overlap_chars: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
            overlap_chars: DEFAULT_OVERLAP_CHARS,
        }
    }
}

/// Find the best cut position using scored break points with squared distance decay.
fn find_best_cutoff(
    break_points: &[BreakPoint],
    target_pos: usize,
    window_chars: usize,
    code_fences: &[Range<usize>],
) -> usize {
    let window_start = target_pos.saturating_sub(window_chars);
    let mut best_score: f64 = -1.0;
    let mut best_pos = target_pos;

    for bp in break_points {
        if bp.pos < window_start {
            continue;
        }
        if bp.pos > target_pos {
            break;
        }

        if is_inside_code_fence(bp.pos, code_fences) {
            continue;
        }

        let distance = target_pos - bp.pos;
        let normalized_dist = distance as f64 / window_chars as f64;
        // Squared decay: gentle early, steep late
        let multiplier = 1.0 - (normalized_dist * normalized_dist) * DECAY_FACTOR;
        let final_score = bp.score as f64 * multiplier;

        if final_score > best_score {
            best_score = final_score;
            best_pos = bp.pos;
        }
    }

    best_pos
}

/// Chunk a document intelligently, respecting markdown structure.
///
/// Returns an error if the input exceeds `MAX_INPUT_CHARS`.
pub(crate) fn chunk_document(content: &str, options: &ChunkOptions) -> Result<Vec<Chunk>, String> {
    if content.len() > MAX_INPUT_CHARS {
        return Err(format!(
            "CHUNK_TEXT input exceeds maximum size: {} bytes (limit {})",
            content.len(),
            MAX_INPUT_CHARS,
        ));
    }

    if content.is_empty() {
        return Ok(Vec::new());
    }

    if content.len() <= options.max_chars {
        return Ok(vec![Chunk {
            text: content.to_string(),
            pos: 0,
            index: 0,
        }]);
    }

    let break_points = scan_break_points(content);
    let code_fences = find_code_fences(content);

    let mut chunks = Vec::new();
    let mut char_pos = 0;
    let mut index = 0;

    while char_pos < content.len() {
        let target_end = std::cmp::min(char_pos + options.max_chars, content.len());
        let mut end_pos = target_end;

        // Find best break point if not at end of document
        if end_pos < content.len() {
            let best_cutoff = find_best_cutoff(
                &break_points,
                target_end,
                DEFAULT_WINDOW_CHARS,
                &code_fences,
            );

            if best_cutoff > char_pos && best_cutoff <= target_end {
                end_pos = best_cutoff;
            }
        }

        // Ensure progress
        if end_pos <= char_pos {
            end_pos = std::cmp::min(char_pos + options.max_chars, content.len());
        }

        chunks.push(Chunk {
            text: content[char_pos..end_pos].to_string(),
            pos: char_pos,
            index,
        });
        index += 1;

        if end_pos >= content.len() {
            break;
        }

        // Move forward with overlap
        let new_pos = end_pos.saturating_sub(options.overlap_chars);
        if new_pos <= char_pos {
            char_pos = end_pos; // Prevent infinite loop
        } else {
            char_pos = new_pos;
        }
    }

    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_document_returns_single_chunk() {
        let text = "Hello, world!";
        let chunks = chunk_document(text, &ChunkOptions::default()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].pos, 0);
        assert_eq!(chunks[0].index, 0);
    }

    #[test]
    fn empty_document_returns_empty() {
        let chunks = chunk_document("", &ChunkOptions::default()).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn large_input_rejected() {
        let text = "x".repeat(MAX_INPUT_CHARS + 1);
        let result = chunk_document(&text, &ChunkOptions::default());
        assert!(result.is_err());
    }

    #[test]
    fn chunks_have_sequential_indices() {
        let text = "a".repeat(8000); // Will produce multiple chunks with default settings
        let chunks = chunk_document(&text, &ChunkOptions::default()).unwrap();
        assert!(chunks.len() > 1);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
        }
    }

    #[test]
    fn prefers_heading_break_points() {
        // Create a document where a heading falls within the break window
        let section1 = "a".repeat(3200);
        let text = format!("{}\n## Section 2\n{}", section1, "b".repeat(2000));
        let opts = ChunkOptions {
            max_chars: 3600,
            overlap_chars: 540,
        };
        let chunks = chunk_document(&text, &opts).unwrap();
        assert!(chunks.len() >= 2);
        // First chunk should end at or before the heading
        assert!(chunks[0].text.len() <= 3600);
    }

    #[test]
    fn does_not_split_inside_code_fence() {
        let code = "x".repeat(100);
        let text = format!("intro\n```\n{}\n```\nafter\n\n{}", code, "y".repeat(4000));
        let opts = ChunkOptions {
            max_chars: 200,
            overlap_chars: 30,
        };
        let chunks = chunk_document(&text, &opts).unwrap();
        // Verify no chunk boundary falls inside the code fence
        let fence_start = text.find("\n```\n").unwrap();
        let fence_end = text[fence_start + 4..]
            .find("\n```")
            .map(|p| p + fence_start + 4 + 4)
            .unwrap();
        for chunk in &chunks {
            let chunk_end = chunk.pos + chunk.text.len();
            // chunk boundaries should not be strictly inside the fence
            if chunk_end > fence_start && chunk_end < fence_end && chunk.pos < fence_start {
                panic!(
                    "Chunk boundary at {} falls inside code fence {}..{}",
                    chunk_end, fence_start, fence_end
                );
            }
        }
    }

    #[test]
    fn custom_options() {
        let text = "a".repeat(500);
        let opts = ChunkOptions {
            max_chars: 200,
            overlap_chars: 30,
        };
        let chunks = chunk_document(&text, &opts).unwrap();
        assert!(chunks.len() > 1);
        // Each chunk should be at most max_chars
        for chunk in &chunks {
            assert!(chunk.text.len() <= 200);
        }
    }
}
