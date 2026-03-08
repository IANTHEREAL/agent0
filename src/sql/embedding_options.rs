pub(crate) fn parse_embed_text_json_options_dimensions(opts: &str) -> Result<Option<u32>, String> {
    let json: serde_json::Value = serde_json::from_str(opts)
        .map_err(|e| format!("embed_text: invalid JSON options: {}", e))?;
    let json = json
        .as_object()
        .ok_or_else(|| "embed_text: JSON options must be an object".to_string())?;

    match json.get("dimensions") {
        Some(serde_json::Value::Number(n)) => {
            let d = n
                .as_u64()
                .ok_or_else(|| "embed_text: dimensions must be a positive integer".to_string())?;
            if d == 0 || d > u32::MAX as u64 {
                return Err("embed_text: dimensions out of range".to_string());
            }
            Ok(Some(d as u32))
        }
        Some(_) => Err("embed_text: dimensions must be a number".to_string()),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_embed_text_json_options_dimensions;

    #[test]
    fn parse_embed_text_json_options_dimensions_accepts_missing_dimensions() {
        assert_eq!(
            parse_embed_text_json_options_dimensions("{\"foo\":1}").unwrap(),
            None
        );
    }

    #[test]
    fn parse_embed_text_json_options_dimensions_accepts_numeric_dimensions() {
        assert_eq!(
            parse_embed_text_json_options_dimensions("{\"dimensions\":1024}").unwrap(),
            Some(1024)
        );
    }

    #[test]
    fn parse_embed_text_json_options_dimensions_rejects_malformed_json() {
        let err = parse_embed_text_json_options_dimensions("{not-json").unwrap_err();
        assert!(err.contains("invalid JSON options"));
    }

    #[test]
    fn parse_embed_text_json_options_dimensions_rejects_non_numeric_dimensions() {
        let err =
            parse_embed_text_json_options_dimensions("{\"dimensions\":\"1024\"}").unwrap_err();
        assert_eq!(err, "embed_text: dimensions must be a number");
    }

    #[test]
    fn parse_embed_text_json_options_dimensions_rejects_non_object_json() {
        let err = parse_embed_text_json_options_dimensions("123").unwrap_err();
        assert_eq!(err, "embed_text: JSON options must be an object");
    }
}
