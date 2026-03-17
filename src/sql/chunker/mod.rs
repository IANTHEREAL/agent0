//! Smart document chunking for vector embeddings.
//!
//! Markdown-aware chunking algorithm ported from QMD (<https://github.com/tobi/qmd>).
//! Respects document structure (headings, code blocks) and uses a scoring system
//! to find optimal break points.

mod break_points;
mod code_fences;
mod splitter;
mod title;

pub(crate) use splitter::{chunk_document, ChunkOptions, DEFAULT_MAX_CHARS, DEFAULT_OVERLAP_CHARS};
pub(crate) use title::format_for_embedding;
