//! BEFORE trigger body compilation cache.

use anyhow::Result;
use dashmap::DashMap;

#[derive(Clone, Debug)]
pub(super) enum TriggerStatement {
    ReturnNew,
    ReturnNull,
    ReturnOld,
    Assignment { column: String, expr_str: String },
    Skip,
}

#[derive(Clone, Debug)]
pub(super) struct CompiledTriggerBody {
    pub(super) statements: Vec<TriggerStatement>,
}

/// Per-tenant cache for compiled trigger function bodies.
///
/// Owned by `TenantEntry` in the pool — when the reaper drops the entry,
/// the cache is dropped automatically (no manual eviction needed).
pub(crate) struct TriggerBodyCache {
    inner: DashMap<(u64, u32), CompiledTriggerBody>, // (db_id, func_oid)
}

impl TriggerBodyCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }

    pub(super) fn get_or_compile(
        &self,
        db_id: u64,
        func_oid: u32,
        body: &str,
    ) -> Result<CompiledTriggerBody> {
        let key = (db_id, func_oid);
        if let Some(compiled) = self.inner.get(&key) {
            return Ok(compiled.clone());
        }

        let compiled = CompiledTriggerBody::compile(body)?;
        self.inner.insert(key, compiled.clone());
        Ok(compiled)
    }

    /// Remove all compiled trigger bodies for a database.
    ///
    /// Called on DDL that changes function definitions (CREATE OR REPLACE
    /// FUNCTION, DROP FUNCTION). Invalidation is coarse-grained by db_id
    /// because DDL is rare and entries are cheap to recompile on next use.
    pub(crate) fn invalidate_db(&self, db_id: u64) {
        self.inner.retain(|&(did, _), _| did != db_id);
    }
}

const UNSUPPORTED_FTS_FUNCTIONS: &[&str] = &["tsvector_update_trigger"];

fn validate_trigger_body(body: &str) -> Result<()> {
    let body_lower = body.to_lowercase();
    for func in UNSUPPORTED_FTS_FUNCTIONS {
        if let Some(pos) = body_lower.find(func) {
            let after_pos = pos + func.len();
            if after_pos < body_lower.len() {
                let next_char = body_lower.as_bytes()[after_pos] as char;
                if next_char == '(' || next_char.is_whitespace() {
                    return Err(anyhow::anyhow!(
                        "Trigger uses unsupported function '{}'. \
                         Use to_tsvector/plainto_tsquery/ts_rank instead.",
                        func
                    ));
                }
            }
        }
    }
    Ok(())
}

impl CompiledTriggerBody {
    fn compile(body: &str) -> Result<Self> {
        validate_trigger_body(body)?;

        let body_upper = body.to_ascii_uppercase();
        let begin_pos = match body_upper.find("BEGIN") {
            Some(p) => p + 5,
            None => return Ok(Self { statements: vec![] }),
        };
        let end_pos = body_upper.rfind("END").unwrap_or(body.len());
        let block_content = &body[begin_pos..end_pos];

        let normalized = block_content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with("--"))
            .collect::<Vec<_>>()
            .join(" ");

        let mut statements = Vec::new();
        for stmt in normalized.split(';') {
            let line = stmt.trim();
            if line.is_empty() {
                continue;
            }

            let line_upper = line.to_uppercase();

            let parsed = if line_upper.starts_with("RETURN NEW") {
                TriggerStatement::ReturnNew
            } else if line_upper.starts_with("RETURN NULL") {
                TriggerStatement::ReturnNull
            } else if line_upper.starts_with("RETURN OLD") {
                TriggerStatement::ReturnOld
            } else if line_upper.starts_with("NEW.") {
                Self::parse_assignment(line)?
            } else {
                TriggerStatement::Skip
            };
            statements.push(parsed);
        }

        Ok(Self { statements })
    }

    fn parse_assignment(line: &str) -> Result<TriggerStatement> {
        let assignment = &line[4..];

        let (col_name, expr_str) = if let Some(pos) = assignment.find(":=") {
            (
                assignment[..pos].trim(),
                assignment[pos + 2..].trim().trim_end_matches(';'),
            )
        } else if let Some(pos) = assignment.find('=') {
            let before = assignment.chars().nth(pos.saturating_sub(1));
            let after = assignment.chars().nth(pos + 1);
            if before == Some(':')
                || before == Some('<')
                || before == Some('>')
                || before == Some('!')
                || after == Some('=')
            {
                return Ok(TriggerStatement::Skip);
            }
            (
                assignment[..pos].trim(),
                assignment[pos + 1..].trim().trim_end_matches(';'),
            )
        } else {
            return Ok(TriggerStatement::Skip);
        };

        Ok(TriggerStatement::Assignment {
            column: col_name.to_string(),
            expr_str: expr_str.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_trigger_body_allows_normal_triggers() {
        let body = r#"
            BEGIN
                NEW.updated_at := NOW();
                RETURN NEW;
            END;
        "#;
        assert!(validate_trigger_body(body).is_ok());
    }

    #[test]
    fn test_validate_trigger_body_allows_to_tsvector() {
        let body = r#"
            BEGIN
                NEW.search_vector := to_tsvector('english', NEW.title);
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_ok(), "to_tsvector should be supported now");
    }

    #[test]
    fn test_validate_trigger_body_allows_setweight() {
        let body = r#"
            BEGIN
                NEW.search_vector := setweight(to_tsvector('english', NEW.title), 'A');
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_ok(), "setweight should be supported now");
    }

    #[test]
    fn test_validate_trigger_body_detects_unsupported_fts() {
        let body = r#"
            BEGIN
                NEW.search_vector := tsvector_update_trigger();
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("tsvector_update_trigger"));
    }

    #[test]
    fn test_validate_trigger_body_ignores_similar_names() {
        let body = r#"
            BEGIN
                NEW.to_tsvector_count := 1;
                RETURN NEW;
            END;
        "#;
        assert!(validate_trigger_body(body).is_ok());
    }

    #[test]
    fn test_trigger_body_cache_instance_isolation() {
        let body_a = "BEGIN\n  NEW.x := 1;\n  RETURN NEW;\nEND;";
        let body_b = "BEGIN\n  NEW.x := 999;\n  RETURN NEW;\nEND;";
        let oid = 42u32;
        let db_id = 1u64;

        let cache_a = TriggerBodyCache::new();
        let cache_b = TriggerBodyCache::new();

        // Same (db_id, func_oid), different cache instances -> independent entries
        let a = cache_a.get_or_compile(db_id, oid, body_a).unwrap();
        let b = cache_b.get_or_compile(db_id, oid, body_b).unwrap();

        let a_expr = match &a.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        let b_expr = match &b.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(a_expr, "1");
        assert_eq!(b_expr, "999");

        // Re-fetch from cache_a returns the correct body
        let a2 = cache_a.get_or_compile(db_id, oid, body_a).unwrap();
        let a2_expr = match &a2.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(a2_expr, "1");
    }

    #[test]
    fn test_trigger_body_cache_db_id_isolation() {
        let cache = TriggerBodyCache::new();
        let oid = 42u32;

        let body_a = "BEGIN\n  NEW.x := 1;\n  RETURN NEW;\nEND;";
        let body_b = "BEGIN\n  NEW.x := 50;\n  RETURN NEW;\nEND;";

        let a = cache.get_or_compile(1, oid, body_a).unwrap();
        let b = cache.get_or_compile(2, oid, body_b).unwrap();

        let a_expr = match &a.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        let b_expr = match &b.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(a_expr, "1");
        assert_eq!(b_expr, "50");
    }

    #[test]
    fn test_invalidate_db_clears_stale_entries() {
        // Simulates CREATE OR REPLACE FUNCTION: same (db_id, func_oid),
        // DDL invalidates, next call recompiles with new body.
        let cache = TriggerBodyCache::new();
        let db_id = 1u64;
        let oid = 10u32;

        let body_v1 = "BEGIN\n  NEW.x := 1;\n  RETURN NEW;\nEND;";
        let body_v2 = "BEGIN\n  NEW.x := 42;\n  RETURN NEW;\nEND;";

        // Populate cache with v1
        let v1 = cache.get_or_compile(db_id, oid, body_v1).unwrap();
        let v1_expr = match &v1.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(v1_expr, "1");

        // Without invalidation, cache returns stale v1
        let stale = cache.get_or_compile(db_id, oid, body_v2).unwrap();
        let stale_expr = match &stale.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(stale_expr, "1"); // stale!

        // DDL invalidation clears entries for this db
        cache.invalidate_db(db_id);

        // Now the cache miss forces recompilation with v2
        let v2 = cache.get_or_compile(db_id, oid, body_v2).unwrap();
        let v2_expr = match &v2.statements[0] {
            TriggerStatement::Assignment { expr_str, .. } => expr_str.clone(),
            other => panic!("expected Assignment, got {:?}", other),
        };
        assert_eq!(v2_expr, "42");
    }

    #[test]
    fn test_invalidate_db_does_not_affect_other_dbs() {
        let cache = TriggerBodyCache::new();
        let body = "BEGIN\n  NEW.x := 1;\n  RETURN NEW;\nEND;";

        cache.get_or_compile(1, 10, body).unwrap();
        cache.get_or_compile(2, 10, body).unwrap();

        cache.invalidate_db(1);

        // db_id=1 entry is gone (cache miss -> recompile)
        assert!(cache.inner.get(&(1u64, 10u32)).is_none());
        // db_id=2 entry is still present
        assert!(cache.inner.get(&(2u64, 10u32)).is_some());
    }
}
