//! Title extraction and embedding format helpers.

/// Format a chunk for embedding with title prefix.
pub(crate) fn format_for_embedding(text: &str, title: &str) -> String {
    format!("title: {} | text: {}", title, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::sync::LazyLock;

    static H1_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#\s+(.+)$").unwrap());
    static H2_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^##\s+(.+)$").unwrap());

    fn extract_title(content: &str, fallback: Option<&str>) -> String {
        if let Some(m) = H1_RE.captures(content) {
            return m[1].trim().to_string();
        }
        if let Some(m) = H2_RE.captures(content) {
            return m[1].trim().to_string();
        }
        if let Some(first_line) = content.lines().next() {
            let trimmed = first_line.trim();
            if !trimmed.is_empty() && trimmed.len() < 100 {
                return trimmed.to_string();
            }
        }
        fallback.unwrap_or("Untitled").to_string()
    }

    #[test]
    fn extracts_h1_title() {
        assert_eq!(extract_title("# My Title\nContent", None), "My Title");
    }

    #[test]
    fn extracts_h2_when_no_h1() {
        assert_eq!(extract_title("## Subtitle\nContent", None), "Subtitle");
    }

    #[test]
    fn uses_first_line_when_no_headings() {
        assert_eq!(
            extract_title("Short first line\nMore text", None),
            "Short first line"
        );
    }

    #[test]
    fn uses_fallback_for_empty() {
        assert_eq!(extract_title("", Some("fallback.md")), "fallback.md");
    }

    #[test]
    fn uses_untitled_default() {
        assert_eq!(extract_title("", None), "Untitled");
    }

    #[test]
    fn format_embedding() {
        let result = format_for_embedding("hello world", "My Doc");
        assert_eq!(result, "title: My Doc | text: hello world");
    }
}
