//! Collation support for text comparison and sorting.
//!
//! This module provides:
//! - ICU-based collation for locale-specific text ordering
//! - `ResolvedCollation` enum embedded in the typed IR (resolved at analysis time)
//! - Process-level ICU collator cache (keyed by locale string)
//! - Collation registry for managing CREATE COLLATION definitions

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
}

// ── Collation registry (process-global, for DDL definitions) ────────

/// Global collation registry
pub static COLLATION_REGISTRY: Lazy<CollationRegistry> = Lazy::new(CollationRegistry::new);

pub struct CollationRegistry {
    // Map from collation name to collation definition
    definitions: DashMap<String, CollationDef>,
}

impl CollationRegistry {
    fn new() -> Self {
        let registry = Self {
            definitions: DashMap::new(),
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

    /// Get a collation definition
    pub fn get_definition(&self, name: &str) -> Option<CollationDef> {
        self.definitions.get(name).map(|entry| entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
