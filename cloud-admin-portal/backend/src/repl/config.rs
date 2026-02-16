use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct FileConfig {
    pub repl: Option<ReplConfig>,
    pub startup: Option<StartupConfig>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReplConfig {
    pub pager: Option<bool>,
    pub pager_command: Option<String>,
    /// "off", "on", "auto"
    pub expanded: Option<String>,
    /// String to display for NULL values (default: "NULL")
    pub null_display: Option<String>,
    /// Border style: 0 (none), 1 (single), 2 (double) — default 1
    pub border: Option<u8>,
    /// Line drawing style: "ascii" or "unicode" — default "ascii"
    pub linestyle: Option<String>,
    /// Show query timing (default: true)
    pub timing: Option<bool>,
    /// Enable syntax highlighting (default: true)
    pub highlight: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StartupConfig {
    pub commands: Option<Vec<String>>,
}

/// Load config from ~/.db9/config.toml.
/// Missing file → silent defaults. Invalid TOML → warning + defaults.
pub fn load_config() -> FileConfig {
    let path = match config_path() {
        Some(p) => p,
        None => return FileConfig::default(),
    };

    if !path.exists() {
        return FileConfig::default();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str::<FileConfig>(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "Warning: failed to parse {}: {}. Using defaults.",
                    path.display(),
                    e
                );
                FileConfig::default()
            }
        },
        Err(e) => {
            eprintln!(
                "Warning: failed to read {}: {}. Using defaults.",
                path.display(),
                e
            );
            FileConfig::default()
        }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".db9").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> FileConfig {
        toml::from_str::<FileConfig>(toml_str).unwrap()
    }

    #[test]
    fn test_full_config() {
        let cfg = parse(
            r#"
[repl]
pager = false
pager_command = "less -R"
expanded = "auto"
null_display = "∅"
border = 2
linestyle = "unicode"
timing = false
highlight = false

[startup]
commands = ["\\dt", "\\timing off"]
"#,
        );

        let r = cfg.repl.unwrap();
        assert_eq!(r.pager, Some(false));
        assert_eq!(r.pager_command.as_deref(), Some("less -R"));
        assert_eq!(r.expanded.as_deref(), Some("auto"));
        assert_eq!(r.null_display.as_deref(), Some("∅"));
        assert_eq!(r.border, Some(2));
        assert_eq!(r.linestyle.as_deref(), Some("unicode"));
        assert_eq!(r.timing, Some(false));
        assert_eq!(r.highlight, Some(false));

        let s = cfg.startup.unwrap();
        let cmds = s.commands.unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], "\\dt");
        assert_eq!(cmds[1], "\\timing off");
    }

    #[test]
    fn test_partial_config_defaults() {
        let cfg = parse(
            r#"
[repl]
border = 0
"#,
        );

        let r = cfg.repl.unwrap();
        assert_eq!(r.border, Some(0));
        assert_eq!(r.pager, None);
        assert_eq!(r.pager_command, None);
        assert_eq!(r.expanded, None);
        assert_eq!(r.null_display, None);
        assert_eq!(r.linestyle, None);
        assert_eq!(r.timing, None);
        assert_eq!(r.highlight, None);
        assert!(cfg.startup.is_none());
    }

    #[test]
    fn test_empty_string_parses_to_defaults() {
        let cfg = parse("");
        assert!(cfg.repl.is_none());
        assert!(cfg.startup.is_none());
    }

    #[test]
    fn test_invalid_toml_returns_defaults() {
        let result = toml::from_str::<FileConfig>("this is not valid toml {{{{");
        assert!(result.is_err());
        let fallback = FileConfig::default();
        assert!(fallback.repl.is_none());
        assert!(fallback.startup.is_none());
    }

    #[test]
    fn test_unknown_keys_ignored() {
        let cfg = parse(
            r#"
[repl]
border = 1
some_future_key = "value"

[some_future_section]
x = 42
"#,
        );
        let r = cfg.repl.unwrap();
        assert_eq!(r.border, Some(1));
    }

    #[test]
    fn test_border_out_of_range_still_parses() {
        let cfg = parse(
            r#"
[repl]
border = 255
"#,
        );
        assert_eq!(cfg.repl.unwrap().border, Some(255));
    }

    #[test]
    fn test_startup_empty_commands() {
        let cfg = parse(
            r#"
[startup]
commands = []
"#,
        );
        let cmds = cfg.startup.unwrap().commands.unwrap();
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_config_path_exists() {
        let path = config_path();
        if let Some(p) = path {
            assert!(p.ends_with(".db9/config.toml"));
        }
    }
}
