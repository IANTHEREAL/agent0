//! Collation support for text comparison and sorting.
//!
//! This module provides:
//! - ICU-based collation for locale-specific text ordering
//! - `ResolvedCollation` enum embedded in the typed IR (resolved at analysis time)
//! - Process-level ICU collator cache (keyed by locale string)
//! - Collation registry for managing CREATE COLLATION definitions
//! - Sort key generation for ORDER BY operations
//! - Collation-aware text comparison

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rust_icu_ucol as ucol;
use rust_icu_ustring as ustring;
use std::sync::Arc;

// ── Resolved collation (embedded in typed IR) ──────────────────────

/// A fully-resolved collation descriptor, embedded in `TypedExprKind::Collate`
/// at analysis time. The runtime never needs to look up collation definitions
/// by name — it uses this descriptor directly.
///
/// - `None` (when wrapped in `Option<ResolvedCollation>`) = no collation (use
///   default `compare_text_pg`).
/// - `Some(Binary)` = explicit C/POSIX — byte ordering (`str::cmp`).
/// - `Some(Icu(locale))` = ICU locale — use collator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ResolvedCollation {
    /// C / POSIX — byte-wise ordering via `str::cmp`.
    Binary,
    /// ICU locale string, e.g. "da-u-kf-lower", "de".
    Icu(String),
}

/// Resolve a collation name to a `ResolvedCollation` using the global registry.
///
/// Built-in binary collations (C, POSIX) are resolved without registry lookup.
/// For user-defined collations, the registry is consulted.
pub fn resolve_collation_from_registry(name: &str) -> Result<ResolvedCollation> {
    // Built-in binary collations
    if name.eq_ignore_ascii_case("c") || name.eq_ignore_ascii_case("posix") {
        return Ok(ResolvedCollation::Binary);
    }
    // "default" — use the database default (no special collation)
    if name.eq_ignore_ascii_case("default") {
        return Ok(ResolvedCollation::Binary);
    }
    // Registry lookup
    match COLLATION_REGISTRY.get_definition(name) {
        Some(def) if def.provider == "icu" => {
            let locale = def
                .locale
                .as_ref()
                .ok_or_else(|| anyhow!("ICU collation '{}' has no locale", name))?;
            Ok(ResolvedCollation::Icu(locale.clone()))
        }
        Some(def) if def.provider == "c" => Ok(ResolvedCollation::Binary),
        Some(_) => Ok(ResolvedCollation::Binary),
        None => Err(anyhow!("collation \"{}\" does not exist", name)),
    }
}

// ── Collator cache (process-level, keyed by ICU locale) ─────────────

/// Process-level ICU collator cache, keyed by locale string.
/// `ResolvedCollation` is self-describing, so this cache is safe to share
/// across databases/tenants.
pub static COLLATOR_CACHE: Lazy<CollatorCache> = Lazy::new(CollatorCache::new);

pub struct CollatorCache {
    collators: DashMap<String, Arc<Collator>>,
}

impl CollatorCache {
    fn new() -> Self {
        Self {
            collators: DashMap::new(),
        }
    }

    pub fn get_or_create(&self, locale: &str) -> Result<Arc<Collator>> {
        if let Some(c) = self.collators.get(locale) {
            return Ok(c.clone());
        }
        let c = Arc::new(Collator::new(locale)?);
        self.collators.insert(locale.to_string(), c.clone());
        Ok(c)
    }
}

/// Compare two strings using a `ResolvedCollation`.
pub fn compare_with_resolved_collation(
    a: &str,
    b: &str,
    resolved: &ResolvedCollation,
) -> Result<i32> {
    match resolved {
        ResolvedCollation::Binary => Ok(match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }),
        ResolvedCollation::Icu(locale) => {
            let collator = COLLATOR_CACHE.get_or_create(locale)?;
            collator.compare(a, b)
        }
    }
}

// ── Collation definition (catalog/storage) ──────────────────────────

/// Collation definition stored in catalog
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CollationDef {
    pub name: String,
    pub provider: String, // "icu", "c", "d" (default)
    pub locale: Option<String>,
    pub deterministic: bool,
}

/// Runtime collator instance
pub struct Collator {
    inner: ucol::UCollator,
}

// SAFETY: ICU collators are thread-safe for read operations
// The UCollator internally uses immutable data structures after creation
unsafe impl Send for Collator {}
unsafe impl Sync for Collator {}

impl Collator {
    /// Create a new collator for the given locale
    pub fn new(locale: &str) -> Result<Self> {
        let col = ucol::UCollator::try_from(locale).map_err(|e| {
            anyhow!(
                "Failed to create ICU collator for locale '{}': {}",
                locale,
                e
            )
        })?;
        Ok(Self { inner: col })
    }

    /// Compare two strings using this collation
    /// Returns: -1 if a < b, 0 if a == b, 1 if a > b
    pub fn compare(&self, a: &str, b: &str) -> Result<i32> {
        let a_ustr = ustring::UChar::try_from(a)
            .map_err(|e| anyhow!("Failed to convert string to UChar: {}", e))?;
        let b_ustr = ustring::UChar::try_from(b)
            .map_err(|e| anyhow!("Failed to convert string to UChar: {}", e))?;

        let result = self.inner.strcoll(&a_ustr, &b_ustr);

        Ok(match result {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }

    /// Generate a sort key for the given string
    /// Sort keys can be compared byte-wise to get the same ordering as compare()
    pub fn sort_key(&self, s: &str) -> Result<Vec<u8>> {
        let ustr = ustring::UChar::try_from(s)
            .map_err(|e| anyhow!("Failed to convert string to UChar: {}", e))?;

        let key = self.inner.get_sort_key(&ustr);

        Ok(key)
    }
}

// ── Collation registry (process-global, for DDL definitions) ────────

/// Global collation registry
pub static COLLATION_REGISTRY: Lazy<CollationRegistry> = Lazy::new(CollationRegistry::new);

pub struct CollationRegistry {
    // Map from collation name to collation definition
    definitions: DashMap<String, CollationDef>,
    // Cache of collator instances
    collators: DashMap<String, Arc<Collator>>,
}

impl CollationRegistry {
    fn new() -> Self {
        let registry = Self {
            definitions: DashMap::new(),
            collators: DashMap::new(),
        };

        // Register built-in collations
        registry.definitions.insert(
            "default".to_string(),
            CollationDef {
                name: "default".to_string(),
                provider: "d".to_string(),
                locale: None,
                deterministic: true,
            },
        );
        registry.definitions.insert(
            "C".to_string(),
            CollationDef {
                name: "C".to_string(),
                provider: "c".to_string(),
                locale: Some("C".to_string()),
                deterministic: true,
            },
        );
        registry.definitions.insert(
            "POSIX".to_string(),
            CollationDef {
                name: "POSIX".to_string(),
                provider: "c".to_string(),
                locale: Some("POSIX".to_string()),
                deterministic: true,
            },
        );

        registry
    }

    /// Register a new collation
    pub fn register(&self, def: CollationDef) {
        self.definitions.insert(def.name.clone(), def);
    }

    /// Unregister a collation
    pub fn unregister(&self, name: &str) -> Option<CollationDef> {
        self.collators.remove(name);
        self.definitions.remove(name).map(|(_, def)| def)
    }

    /// Get a collation definition
    pub fn get_definition(&self, name: &str) -> Option<CollationDef> {
        self.definitions.get(name).map(|entry| entry.clone())
    }

    /// Get all collation definitions
    pub fn all_definitions(&self) -> Vec<CollationDef> {
        self.definitions
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get or create a collator for the given collation name
    pub fn get_collator(&self, name: &str) -> Result<Arc<Collator>> {
        // Check cache first
        if let Some(collator) = self.collators.get(name) {
            return Ok(collator.clone());
        }

        // Get definition
        let def = self
            .get_definition(name)
            .ok_or_else(|| anyhow!("Collation '{}' does not exist", name))?;

        // Only ICU provider is supported for now
        if def.provider != "icu" {
            return Err(anyhow!(
                "Only ICU collations are supported for collation-aware operations"
            ));
        }

        let locale = def
            .locale
            .as_ref()
            .ok_or_else(|| anyhow!("ICU collation '{}' has no locale", name))?;

        // Create collator
        let collator = Arc::new(Collator::new(locale)?);
        self.collators.insert(name.to_string(), collator.clone());
        Ok(collator)
    }

    /// Check if a collation exists (case-insensitive for built-in names)
    pub fn exists(&self, name: &str) -> bool {
        if self.definitions.contains_key(name) {
            return true;
        }
        // Case-insensitive check for built-in names
        let lower = name.to_lowercase();
        matches!(lower.as_str(), "c" | "posix" | "default")
    }

    /// Check if a collation is binary-safe (can use index ordering)
    pub fn is_binary_safe(&self, name: &str) -> bool {
        // Only C/POSIX/default are binary-safe
        matches!(name, "C" | "POSIX" | "default")
    }
}

/// Compare two text values using the specified collation
pub fn compare_with_collation(a: &str, b: &str, collation: Option<&str>) -> Result<i32> {
    let collation_name = collation.unwrap_or("default");

    // Fast path for binary-safe collations
    if COLLATION_REGISTRY.is_binary_safe(collation_name) {
        return Ok(match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        });
    }

    // Use ICU collator
    let collator = COLLATION_REGISTRY.get_collator(collation_name)?;
    collator.compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_collation() {
        let result = compare_with_collation("a", "A", Some("C")).unwrap();
        assert!(result > 0, "Binary collation: 'a' should sort after 'A'");
    }

    #[test]
    fn test_icu_da_collation() {
        // Register Danish collation
        COLLATION_REGISTRY.register(CollationDef {
            name: "da".to_string(),
            provider: "icu".to_string(),
            locale: Some("da-u-kf-lower".to_string()),
            deterministic: true,
        });

        let result = compare_with_collation("a", "A", Some("da")).unwrap();
        assert!(
            result < 0,
            "Danish collation: lowercase 'a' should sort before 'A'"
        );
    }

    #[test]
    fn test_icu_de_collation() {
        // Register German collation
        COLLATION_REGISTRY.register(CollationDef {
            name: "de".to_string(),
            provider: "icu".to_string(),
            locale: Some("de".to_string()),
            deterministic: true,
        });

        let result = compare_with_collation("a", "A", Some("de")).unwrap();
        assert!(
            result < 0,
            "German collation: lowercase 'a' should sort before 'A'"
        );
    }

    #[test]
    fn test_resolved_collation_binary() {
        let result = compare_with_resolved_collation("a", "A", &ResolvedCollation::Binary).unwrap();
        assert!(result > 0, "Binary: 'a' > 'A' in byte ordering");
    }

    #[test]
    fn test_resolved_collation_icu() {
        let result = compare_with_resolved_collation(
            "a",
            "A",
            &ResolvedCollation::Icu("da-u-kf-lower".to_string()),
        )
        .unwrap();
        assert!(result < 0, "ICU da: lowercase 'a' before 'A'");
    }

    #[test]
    fn test_resolve_collation_from_registry_builtins() {
        assert_eq!(
            resolve_collation_from_registry("C").unwrap(),
            ResolvedCollation::Binary
        );
        assert_eq!(
            resolve_collation_from_registry("POSIX").unwrap(),
            ResolvedCollation::Binary
        );
        assert_eq!(
            resolve_collation_from_registry("c").unwrap(),
            ResolvedCollation::Binary
        );
    }
}
