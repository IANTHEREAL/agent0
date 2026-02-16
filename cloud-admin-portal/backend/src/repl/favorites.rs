use std::collections::BTreeMap;
use std::path::PathBuf;

/// Manages named query favorites stored in ~/.db9/favorites.toml
pub struct Favorites {
    path: PathBuf,
    queries: BTreeMap<String, String>,
}

impl Favorites {
    /// Load favorites from ~/.db9/favorites.toml
    /// If file doesn't exist, returns empty Favorites
    pub fn load() -> Self {
        let path = Self::get_path();
        let queries = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => Self::parse_toml(&content),
                Err(_) => BTreeMap::new(),
            }
        } else {
            BTreeMap::new()
        };

        Self { path, queries }
    }

    /// Save favorites to ~/.db9/favorites.toml
    pub fn save(&self) -> Result<(), String> {
        // Ensure directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        // Build TOML content
        let mut content = String::from("[favorites]\n");
        for (name, query) in &self.queries {
            // Escape quotes in query
            let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
            content.push_str(&format!("{} = \"{}\"\n", name, escaped));
        }

        std::fs::write(&self.path, content)
            .map_err(|e| format!("Failed to write favorites file: {}", e))
    }

    /// Add or update a favorite query
    pub fn add(&mut self, name: &str, query: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("Favorite name cannot be empty".to_string());
        }
        if query.is_empty() {
            return Err("Query cannot be empty".to_string());
        }
        self.queries.insert(name.to_string(), query.to_string());
        self.save()
    }

    /// Get a favorite query by name
    pub fn get(&self, name: &str) -> Option<String> {
        self.queries.get(name).cloned()
    }

    /// Delete a favorite by name
    pub fn delete(&mut self, name: &str) -> Result<bool, String> {
        let removed = self.queries.remove(name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// List all favorites as (name, query) tuples
    pub fn list(&self) -> Vec<(String, String)> {
        self.queries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get the path to the favorites file
    fn get_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".db9").join("favorites.toml")
    }

    /// Parse TOML [favorites] section
    fn parse_toml(content: &str) -> BTreeMap<String, String> {
        let mut queries = BTreeMap::new();

        let mut in_favorites = false;
        for line in content.lines() {
            let trimmed = line.trim();

            // Check for [favorites] section
            if trimmed == "[favorites]" {
                in_favorites = true;
                continue;
            }

            // Stop if we hit another section
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_favorites = false;
                continue;
            }

            if !in_favorites || trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Parse key = "value"
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim().to_string();
                let value_part = trimmed[eq_pos + 1..].trim();

                // Extract quoted string
                if value_part.starts_with('"') && value_part.ends_with('"') {
                    let quoted = &value_part[1..value_part.len() - 1];
                    // Unescape
                    let unescaped = quoted.replace("\\\"", "\"").replace("\\\\", "\\");
                    queries.insert(key, unescaped);
                }
            }
        }

        queries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_get() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        fav.add("test_query", "SELECT * FROM users").unwrap();
        assert_eq!(
            fav.get("test_query"),
            Some("SELECT * FROM users".to_string())
        );
    }

    #[test]
    fn test_add_empty_name() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        let result = fav.add("", "SELECT * FROM users");
        assert!(result.is_err());
    }

    #[test]
    fn test_add_empty_query() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        let result = fav.add("test", "");
        assert!(result.is_err());
    }

    #[test]
    fn test_delete() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        fav.add("test_query", "SELECT * FROM users").unwrap();
        let deleted = fav.delete("test_query").unwrap();
        assert!(deleted);
        assert_eq!(fav.get("test_query"), None);
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        let deleted = fav.delete("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_list() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/test_favorites.toml"),
            queries: BTreeMap::new(),
        };

        fav.add("query1", "SELECT 1").unwrap();
        fav.add("query2", "SELECT 2").unwrap();

        let list = fav.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "query1");
        assert_eq!(list[1].0, "query2");
    }

    #[test]
    fn test_parse_toml() {
        let content = r#"[favorites]
query1 = "SELECT * FROM users"
query2 = "SELECT COUNT(*) FROM orders"
"#;

        let queries = Favorites::parse_toml(content);
        assert_eq!(queries.len(), 2);
        assert_eq!(
            queries.get("query1"),
            Some(&"SELECT * FROM users".to_string())
        );
        assert_eq!(
            queries.get("query2"),
            Some(&"SELECT COUNT(*) FROM orders".to_string())
        );
    }

    #[test]
    fn test_parse_toml_with_escapes() {
        let content = r#"[favorites]
query1 = "SELECT \"name\" FROM users"
"#;

        let queries = Favorites::parse_toml(content);
        assert_eq!(
            queries.get("query1"),
            Some(&"SELECT \"name\" FROM users".to_string())
        );
    }

    #[test]
    fn test_overwrite_existing_favorite() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/db9_test_fav_overwrite.toml"),
            queries: BTreeMap::new(),
        };
        fav.add("q", "SELECT 1").unwrap();
        fav.add("q", "SELECT 2").unwrap();
        assert_eq!(fav.get("q"), Some("SELECT 2".to_string()));
        let _ = std::fs::remove_file("/tmp/db9_test_fav_overwrite.toml");
    }

    #[test]
    fn test_list_sorted_order() {
        let mut fav = Favorites {
            path: PathBuf::from("/tmp/db9_test_fav_sorted.toml"),
            queries: BTreeMap::new(),
        };
        fav.add("zebra", "SELECT 3").unwrap();
        fav.add("alpha", "SELECT 1").unwrap();
        fav.add("middle", "SELECT 2").unwrap();
        let list = fav.list();
        assert_eq!(list[0].0, "alpha");
        assert_eq!(list[1].0, "middle");
        assert_eq!(list[2].0, "zebra");
        let _ = std::fs::remove_file("/tmp/db9_test_fav_sorted.toml");
    }

    #[test]
    fn test_parse_toml_empty() {
        let queries = Favorites::parse_toml("");
        assert!(queries.is_empty());
    }

    #[test]
    fn test_parse_toml_no_favorites_section() {
        let content = "[other]\nkey = \"value\"\n";
        let queries = Favorites::parse_toml(content);
        assert!(queries.is_empty());
    }

    #[test]
    fn test_save_and_parse_roundtrip() {
        let tmp_path = PathBuf::from("/tmp/db9_test_fav_roundtrip.toml");
        let mut fav = Favorites {
            path: tmp_path.clone(),
            queries: BTreeMap::new(),
        };
        fav.add("q1", "SELECT * FROM users").unwrap();
        fav.add("q2", "INSERT INTO t VALUES (1)").unwrap();
        // Read back the file that save() wrote
        let content = std::fs::read_to_string(&tmp_path).unwrap();
        let parsed = Favorites::parse_toml(&content);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get("q1").map(|s| s.as_str()),
            Some("SELECT * FROM users")
        );
        assert_eq!(
            parsed.get("q2").map(|s| s.as_str()),
            Some("INSERT INTO t VALUES (1)")
        );
        let _ = std::fs::remove_file(&tmp_path);
    }
}
