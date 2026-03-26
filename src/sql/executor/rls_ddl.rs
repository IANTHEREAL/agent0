//! RLS DDL execution: CREATE POLICY, ALTER POLICY, DROP POLICY, ALTER TABLE ... ROW LEVEL SECURITY.
//!
//! These statements bypass sqlparser (which lacks CREATE/DROP POLICY support in 0.40)
//! and are parsed manually, following the same pattern as triggers.rs.

use super::super::names;
use super::super::{ExecuteResult, Session};
use super::core::Executor;
use crate::model::{RlsCommand, RlsPolicy};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::ObjectName;

use super::triggers::strip_leading_sql_comments;

// ── Helpers ──────────────────────────────────────────────────────────────

fn object_name_from_str(s: &str) -> Result<ObjectName> {
    let s = s.trim().trim_end_matches(';');
    if s.is_empty() {
        return Err(anyhow!("Missing object name"));
    }
    // Handle quoted identifiers
    let parts: Vec<&str> = if s.contains('"') {
        // Simple quoted-identifier split: split on '.' but respect double quotes
        split_dotted_ident(s)
    } else {
        s.split('.').collect()
    };
    let idents: Vec<sqlparser::ast::Ident> = parts
        .iter()
        .map(|p| {
            let p = p.trim().trim_matches('"');
            sqlparser::ast::Ident::new(p)
        })
        .collect();
    if idents.is_empty() || idents.iter().any(|i| i.value.is_empty()) {
        return Err(anyhow!("Invalid object name '{}'", s));
    }
    Ok(ObjectName(idents))
}

fn split_dotted_ident(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            '.' if !in_quote => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Consume a keyword (case-insensitive) at the start of `s`, returning the remainder.
fn consume_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() < kw.len() {
        return None;
    }
    if !s[..kw.len()].eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &s[kw.len()..];
    // Must be followed by whitespace, '(', ';', or end of string
    if rest.is_empty()
        || rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '(' || c == ';')
    {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// Consume a SQL identifier (possibly quoted) at the start of `s`.
/// Returns (ident, remainder).
fn consume_ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('"') {
        // Quoted identifier
        let end = rest.find('"')?;
        let ident = rest[..end].to_string();
        Some((ident, rest[end + 1..].trim_start()))
    } else {
        // Unquoted identifier: alphanumeric + underscore
        let end = s
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.')
            .unwrap_or(s.len());
        if end == 0 {
            return None;
        }
        let ident = s[..end].to_string();
        Some((ident, s[end..].trim_start()))
    }
}

/// Find the matching closing parenthesis for a USING/WITH CHECK expression.
/// Handles nested parens.
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_single_quote = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' if !in_single_quote => in_single_quote = true,
            '\'' if in_single_quote => in_single_quote = false,
            '(' if !in_single_quote => depth += 1,
            ')' if !in_single_quote => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract expression from `(expr)` — the outer parens are stripped.
fn extract_paren_expr(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    if !s.starts_with('(') {
        return Err(anyhow!("Expected '(' before expression"));
    }
    let close = find_matching_paren(s).ok_or_else(|| anyhow!("Unmatched parenthesis"))?;
    let expr = s[1..close].trim().to_string();
    if expr.is_empty() {
        return Err(anyhow!("Empty expression in policy"));
    }
    Ok((expr, s[close + 1..].trim_start()))
}

// ── CREATE POLICY parser ─────────────────────────────────────────────────

/// Parsed CREATE POLICY statement.
struct CreatePolicyParsed {
    name: String,
    table: String,
    permissive: bool,
    command: RlsCommand,
    roles: Vec<String>,
    using_expr: Option<String>,
    with_check_expr: Option<String>,
}

fn parse_create_policy_sql(sql: &str) -> Result<CreatePolicyParsed> {
    let sql = strip_leading_sql_comments(sql);
    let rest = consume_keyword(sql, "CREATE").ok_or_else(|| anyhow!("Expected CREATE POLICY"))?;
    let rest = consume_keyword(rest, "POLICY").ok_or_else(|| anyhow!("Expected CREATE POLICY"))?;

    // Policy name
    let (name, rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected policy name after CREATE POLICY"))?;

    // ON table_name
    let rest =
        consume_keyword(rest, "ON").ok_or_else(|| anyhow!("Expected ON after policy name"))?;
    let (table, mut rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected table name after ON"))?;

    // Optional clauses in any order
    let mut permissive = true; // default PERMISSIVE
    let mut command = RlsCommand::All; // default ALL
    let mut roles = Vec::new();
    let mut using_expr = None;
    let mut with_check_expr = None;

    loop {
        let trimmed = rest.trim_start().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            break;
        }

        if let Some(r) = consume_keyword(rest, "AS") {
            if let Some(r2) = consume_keyword(r, "PERMISSIVE") {
                permissive = true;
                rest = r2;
                continue;
            }
            if let Some(r2) = consume_keyword(r, "RESTRICTIVE") {
                permissive = false;
                rest = r2;
                continue;
            }
            return Err(anyhow!("Expected PERMISSIVE or RESTRICTIVE after AS"));
        }

        if let Some(r) = consume_keyword(rest, "FOR") {
            if let Some(r2) = consume_keyword(r, "ALL") {
                command = RlsCommand::All;
                rest = r2;
                continue;
            }
            if let Some(r2) = consume_keyword(r, "SELECT") {
                command = RlsCommand::Select;
                rest = r2;
                continue;
            }
            if let Some(r2) = consume_keyword(r, "INSERT") {
                command = RlsCommand::Insert;
                rest = r2;
                continue;
            }
            if let Some(r2) = consume_keyword(r, "UPDATE") {
                command = RlsCommand::Update;
                rest = r2;
                continue;
            }
            if let Some(r2) = consume_keyword(r, "DELETE") {
                command = RlsCommand::Delete;
                rest = r2;
                continue;
            }
            return Err(anyhow!(
                "Expected ALL, SELECT, INSERT, UPDATE, or DELETE after FOR"
            ));
        }

        if let Some(r) = consume_keyword(rest, "TO") {
            // Parse role list (comma-separated)
            let mut r = r;
            loop {
                let (role, r2) =
                    consume_ident(r).ok_or_else(|| anyhow!("Expected role name after TO"))?;
                roles.push(role.to_lowercase());
                r = r2;
                if r.starts_with(',') {
                    r = r[1..].trim_start();
                } else {
                    break;
                }
            }
            rest = r;
            continue;
        }

        if let Some(r) = consume_keyword(rest, "USING") {
            let (expr, r2) = extract_paren_expr(r)?;
            using_expr = Some(expr);
            rest = r2;
            continue;
        }

        if let Some(r) = consume_keyword(rest, "WITH") {
            let r =
                consume_keyword(r, "CHECK").ok_or_else(|| anyhow!("Expected CHECK after WITH"))?;
            let (expr, r2) = extract_paren_expr(r)?;
            with_check_expr = Some(expr);
            rest = r2;
            continue;
        }

        // If nothing matched, error
        let trimmed = rest.trim().trim_end_matches(';');
        if !trimmed.is_empty() {
            return Err(anyhow!("Unexpected token in CREATE POLICY: '{}'", trimmed));
        }
        break;
    }

    // Validate DDL constraints (PG-compatible)
    match command {
        RlsCommand::Select | RlsCommand::Delete => {
            if with_check_expr.is_some() {
                return Err(anyhow!(
                    "WITH CHECK cannot be applied to SELECT or DELETE policies"
                ));
            }
        }
        RlsCommand::Insert => {
            if using_expr.is_some() {
                return Err(anyhow!("USING cannot be applied to INSERT policies"));
            }
        }
        RlsCommand::All | RlsCommand::Update => {
            // Both USING and WITH CHECK are allowed
        }
    }

    Ok(CreatePolicyParsed {
        name,
        table,
        permissive,
        command,
        roles,
        using_expr,
        with_check_expr,
    })
}

// ── DROP POLICY parser ───────────────────────────────────────────────────

struct DropPolicyParsed {
    if_exists: bool,
    name: String,
    table: String,
}

fn parse_drop_policy_sql(sql: &str) -> Result<DropPolicyParsed> {
    let sql = strip_leading_sql_comments(sql);
    let rest = consume_keyword(sql, "DROP").ok_or_else(|| anyhow!("Expected DROP POLICY"))?;
    let rest = consume_keyword(rest, "POLICY").ok_or_else(|| anyhow!("Expected DROP POLICY"))?;

    let (if_exists, rest) = if let Some(r) = consume_keyword(rest, "IF") {
        let r = consume_keyword(r, "EXISTS").ok_or_else(|| anyhow!("Expected EXISTS after IF"))?;
        (true, r)
    } else {
        (false, rest)
    };

    let (name, rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected policy name after DROP POLICY"))?;

    let rest =
        consume_keyword(rest, "ON").ok_or_else(|| anyhow!("Expected ON after policy name"))?;
    let (table, _rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected table name after ON"))?;

    Ok(DropPolicyParsed {
        if_exists,
        name,
        table,
    })
}

// ── ALTER POLICY parser ──────────────────────────────────────────────────

/// Parsed ALTER POLICY statement.
/// ALTER POLICY <name> ON <table>
///     [TO { <role> | PUBLIC } [, ...]]
///     [USING ( <expr> )]
///     [WITH CHECK ( <expr> )]
struct AlterPolicyParsed {
    name: String,
    table: String,
    roles: Option<Vec<String>>,
    using_expr: Option<Option<String>>, // Some(Some(expr)) = set, Some(None) impossible, None = unchanged
    with_check_expr: Option<Option<String>>,
}

fn parse_alter_policy_sql(sql: &str) -> Result<AlterPolicyParsed> {
    let sql = strip_leading_sql_comments(sql);
    let rest = consume_keyword(sql, "ALTER").ok_or_else(|| anyhow!("Expected ALTER POLICY"))?;
    let rest = consume_keyword(rest, "POLICY").ok_or_else(|| anyhow!("Expected ALTER POLICY"))?;

    // Policy name
    let (name, rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected policy name after ALTER POLICY"))?;

    // ON table_name
    let rest =
        consume_keyword(rest, "ON").ok_or_else(|| anyhow!("Expected ON after policy name"))?;
    let (table, mut rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected table name after ON"))?;

    let mut roles = None;
    let mut using_expr = None;
    let mut with_check_expr = None;

    loop {
        let trimmed = rest.trim_start().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            break;
        }

        if let Some(r) = consume_keyword(rest, "TO") {
            let mut r = r;
            let mut role_list = Vec::new();
            loop {
                let (role, r2) =
                    consume_ident(r).ok_or_else(|| anyhow!("Expected role name after TO"))?;
                role_list.push(role.to_lowercase());
                r = r2;
                if r.starts_with(',') {
                    r = r[1..].trim_start();
                } else {
                    break;
                }
            }
            roles = Some(role_list);
            rest = r;
            continue;
        }

        if let Some(r) = consume_keyword(rest, "USING") {
            let (expr, r2) = extract_paren_expr(r)?;
            using_expr = Some(Some(expr));
            rest = r2;
            continue;
        }

        if let Some(r) = consume_keyword(rest, "WITH") {
            let r =
                consume_keyword(r, "CHECK").ok_or_else(|| anyhow!("Expected CHECK after WITH"))?;
            let (expr, r2) = extract_paren_expr(r)?;
            with_check_expr = Some(Some(expr));
            rest = r2;
            continue;
        }

        let trimmed = rest.trim().trim_end_matches(';');
        if !trimmed.is_empty() {
            return Err(anyhow!("Unexpected token in ALTER POLICY: '{}'", trimmed));
        }
        break;
    }

    if roles.is_none() && using_expr.is_none() && with_check_expr.is_none() {
        return Err(anyhow!(
            "ALTER POLICY must specify at least one of TO, USING, or WITH CHECK"
        ));
    }

    Ok(AlterPolicyParsed {
        name,
        table,
        roles,
        using_expr,
        with_check_expr,
    })
}

// ── ALTER TABLE ... ROW LEVEL SECURITY parser ────────────────────────────

enum AlterTableRlsAction {
    Enable,
    Disable,
    Force,
    NoForce,
}

struct AlterTableRlsParsed {
    table: String,
    action: AlterTableRlsAction,
}

fn parse_alter_table_rls_sql(sql: &str) -> Result<AlterTableRlsParsed> {
    let sql = strip_leading_sql_comments(sql);
    let rest = consume_keyword(sql, "ALTER").ok_or_else(|| anyhow!("Expected ALTER TABLE"))?;
    let rest = consume_keyword(rest, "TABLE").ok_or_else(|| anyhow!("Expected ALTER TABLE"))?;

    let (table, rest) =
        consume_ident(rest).ok_or_else(|| anyhow!("Expected table name after ALTER TABLE"))?;

    // Parse the RLS action
    let action = if let Some(r) = consume_keyword(rest, "ENABLE") {
        let _r = consume_keyword(r, "ROW")
            .and_then(|r| consume_keyword(r, "LEVEL"))
            .and_then(|r| consume_keyword(r, "SECURITY"))
            .ok_or_else(|| anyhow!("Expected ROW LEVEL SECURITY after ENABLE"))?;
        AlterTableRlsAction::Enable
    } else if let Some(r) = consume_keyword(rest, "DISABLE") {
        let _r = consume_keyword(r, "ROW")
            .and_then(|r| consume_keyword(r, "LEVEL"))
            .and_then(|r| consume_keyword(r, "SECURITY"))
            .ok_or_else(|| anyhow!("Expected ROW LEVEL SECURITY after DISABLE"))?;
        AlterTableRlsAction::Disable
    } else if let Some(r) = consume_keyword(rest, "FORCE") {
        let _r = consume_keyword(r, "ROW")
            .and_then(|r| consume_keyword(r, "LEVEL"))
            .and_then(|r| consume_keyword(r, "SECURITY"))
            .ok_or_else(|| anyhow!("Expected ROW LEVEL SECURITY after FORCE"))?;
        AlterTableRlsAction::Force
    } else if let Some(r) = consume_keyword(rest, "NO") {
        let r = consume_keyword(r, "FORCE").ok_or_else(|| anyhow!("Expected FORCE after NO"))?;
        let _r = consume_keyword(r, "ROW")
            .and_then(|r| consume_keyword(r, "LEVEL"))
            .and_then(|r| consume_keyword(r, "SECURITY"))
            .ok_or_else(|| anyhow!("Expected ROW LEVEL SECURITY after NO FORCE"))?;
        AlterTableRlsAction::NoForce
    } else {
        return Err(anyhow!(
            "Expected ENABLE, DISABLE, FORCE, or NO FORCE ROW LEVEL SECURITY"
        ));
    };

    Ok(AlterTableRlsParsed { table, action })
}

// ── Executor implementations ─────────────────────────────────────────────

macro_rules! autocommit_ddl {
    ($session:expr, $body:expr) => {{
        let is_autocommit = !$session.is_in_transaction();
        if is_autocommit {
            $session.begin().await?;
        }
        let result = $body;
        if is_autocommit {
            if result.is_ok() {
                $session.commit().await?;
            } else {
                $session.rollback().await?;
            }
        }
        result
    }};
}

impl Executor {
    pub(crate) async fn execute_create_policy_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let parsed = parse_create_policy_sql(sql)?;
        let table_name_obj = object_name_from_str(&parsed.table)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table_name_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(parsed.table.clone()))?;

                // Get table schema to find table_id
                let schema = self
                    .store()
                    .get_schema(txn, db_id, &table_resolved.full)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(table_resolved.full.clone()))?;

                let policy = RlsPolicy {
                    oid: 0, // Will be assigned by create_policy
                    name: parsed.name.clone(),
                    table_id: schema.table_id,
                    command: parsed.command,
                    permissive: parsed.permissive,
                    roles: if parsed.roles.is_empty() {
                        vec!["public".to_string()]
                    } else {
                        parsed.roles
                    },
                    using_expr: parsed.using_expr,
                    with_check_expr: parsed.with_check_expr,
                };

                self.store().create_policy(txn, db_id, policy).await?;

                // Bump schema version to invalidate plan cache
                let mut updated_schema = schema;
                updated_schema.version += 1;
                self.store()
                    .update_schema(txn, db_id, updated_schema)
                    .await?;

                Ok(ExecuteResult::CommandComplete {
                    tag: "CREATE POLICY",
                })
            }
            .await
        )
    }

    pub(crate) async fn execute_drop_policy_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let parsed = parse_drop_policy_sql(sql)?;
        let table_name_obj = object_name_from_str(&parsed.table)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table_name_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(parsed.table.clone()))?;

                let schema = self
                    .store()
                    .get_schema(txn, db_id, &table_resolved.full)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(table_resolved.full.clone()))?;

                let dropped = self
                    .store()
                    .drop_policy(txn, db_id, schema.table_id, &parsed.name)
                    .await?;

                if !dropped && !parsed.if_exists {
                    return Err(anyhow!(
                        "policy \"{}\" for table \"{}\" does not exist",
                        parsed.name,
                        table_resolved.full
                    ));
                }

                if dropped {
                    // Bump schema version to invalidate plan cache
                    let mut updated_schema = schema;
                    updated_schema.version += 1;
                    self.store()
                        .update_schema(txn, db_id, updated_schema)
                        .await?;
                }

                Ok(ExecuteResult::CommandComplete { tag: "DROP POLICY" })
            }
            .await
        )
    }

    pub(crate) async fn execute_alter_policy_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let parsed = parse_alter_policy_sql(sql)?;
        let table_name_obj = object_name_from_str(&parsed.table)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table_name_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(parsed.table.clone()))?;

                let schema = self
                    .store()
                    .get_schema(txn, db_id, &table_resolved.full)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(table_resolved.full.clone()))?;

                let mut policy = self
                    .store()
                    .get_policy(txn, db_id, schema.table_id, &parsed.name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "policy \"{}\" for table \"{}\" does not exist",
                            parsed.name,
                            table_resolved.full
                        )
                    })?;

                // Validate command-specific constraints (PG-compatible)
                if parsed.using_expr.is_some() && policy.command == RlsCommand::Insert {
                    return Err(anyhow!("USING cannot be applied to INSERT policies"));
                }
                if parsed.with_check_expr.is_some() {
                    match policy.command {
                        RlsCommand::Select | RlsCommand::Delete => {
                            return Err(anyhow!(
                                "WITH CHECK cannot be applied to SELECT or DELETE policies"
                            ));
                        }
                        _ => {}
                    }
                }

                // Update fields that were specified
                if let Some(roles) = parsed.roles {
                    policy.roles = if roles.is_empty() {
                        vec!["public".to_string()]
                    } else {
                        roles
                    };
                }
                if let Some(using) = parsed.using_expr {
                    policy.using_expr = using;
                }
                if let Some(with_check) = parsed.with_check_expr {
                    policy.with_check_expr = with_check;
                }

                self.store().update_policy(txn, db_id, &policy).await?;

                // Bump schema version to invalidate plan cache
                let mut updated_schema = schema;
                updated_schema.version += 1;
                self.store()
                    .update_schema(txn, db_id, updated_schema)
                    .await?;

                Ok(ExecuteResult::CommandComplete {
                    tag: "ALTER POLICY",
                })
            }
            .await
        )
    }

    pub(crate) async fn execute_alter_table_rls_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let parsed = parse_alter_table_rls_sql(sql)?;
        let table_name_obj = object_name_from_str(&parsed.table)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table_name_obj,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(parsed.table.clone()))?;

                let mut schema = self
                    .store()
                    .get_schema(txn, db_id, &table_resolved.full)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(table_resolved.full.clone()))?;

                match parsed.action {
                    AlterTableRlsAction::Enable => schema.rls_enabled = true,
                    AlterTableRlsAction::Disable => schema.rls_enabled = false,
                    AlterTableRlsAction::Force => schema.rls_force = true,
                    AlterTableRlsAction::NoForce => schema.rls_force = false,
                }

                schema.version += 1;
                self.store().update_schema(txn, db_id, schema).await?;

                Ok(ExecuteResult::AlterTable)
            }
            .await
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_policy_basic() {
        let sql = "CREATE POLICY my_policy ON users USING (user_id = current_user)";
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(p.name, "my_policy");
        assert_eq!(p.table, "users");
        assert!(p.permissive);
        assert_eq!(p.command, RlsCommand::All);
        assert_eq!(p.using_expr.as_deref(), Some("user_id = current_user"));
        assert!(p.with_check_expr.is_none());
    }

    #[test]
    fn test_parse_create_policy_full() {
        let sql = r#"CREATE POLICY tenant_isolation ON orders
            AS RESTRICTIVE
            FOR SELECT
            TO app_user, admin
            USING (tenant_id = current_setting('app.tenant_id')::int)"#;
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(p.name, "tenant_isolation");
        assert_eq!(p.table, "orders");
        assert!(!p.permissive);
        assert_eq!(p.command, RlsCommand::Select);
        assert_eq!(p.roles, vec!["app_user", "admin"]);
        assert!(p.using_expr.is_some());
    }

    #[test]
    fn test_parse_create_policy_insert_with_check() {
        let sql = "CREATE POLICY ins_pol ON t FOR INSERT WITH CHECK (x > 0)";
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(p.command, RlsCommand::Insert);
        assert!(p.using_expr.is_none());
        assert_eq!(p.with_check_expr.as_deref(), Some("x > 0"));
    }

    #[test]
    fn test_parse_create_policy_insert_with_using_fails() {
        let sql = "CREATE POLICY bad ON t FOR INSERT USING (x > 0)";
        assert!(parse_create_policy_sql(sql).is_err());
    }

    #[test]
    fn test_parse_create_policy_select_with_check_fails() {
        let sql = "CREATE POLICY bad ON t FOR SELECT WITH CHECK (x > 0)";
        assert!(parse_create_policy_sql(sql).is_err());
    }

    #[test]
    fn test_parse_create_policy_update_both() {
        let sql = "CREATE POLICY upd ON t FOR UPDATE USING (visible) WITH CHECK (x > 0)";
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(p.command, RlsCommand::Update);
        assert_eq!(p.using_expr.as_deref(), Some("visible"));
        assert_eq!(p.with_check_expr.as_deref(), Some("x > 0"));
    }

    #[test]
    fn test_parse_drop_policy_basic() {
        let sql = "DROP POLICY my_policy ON users";
        let p = parse_drop_policy_sql(sql).unwrap();
        assert!(!p.if_exists);
        assert_eq!(p.name, "my_policy");
        assert_eq!(p.table, "users");
    }

    #[test]
    fn test_parse_drop_policy_if_exists() {
        let sql = "DROP POLICY IF EXISTS my_policy ON users;";
        let p = parse_drop_policy_sql(sql).unwrap();
        assert!(p.if_exists);
        assert_eq!(p.name, "my_policy");
        assert_eq!(p.table, "users");
    }

    #[test]
    fn test_parse_alter_table_enable_rls() {
        let sql = "ALTER TABLE users ENABLE ROW LEVEL SECURITY";
        let p = parse_alter_table_rls_sql(sql).unwrap();
        assert_eq!(p.table, "users");
        assert!(matches!(p.action, AlterTableRlsAction::Enable));
    }

    #[test]
    fn test_parse_alter_table_disable_rls() {
        let sql = "ALTER TABLE users DISABLE ROW LEVEL SECURITY;";
        let p = parse_alter_table_rls_sql(sql).unwrap();
        assert_eq!(p.table, "users");
        assert!(matches!(p.action, AlterTableRlsAction::Disable));
    }

    #[test]
    fn test_parse_alter_table_force_rls() {
        let sql = "ALTER TABLE users FORCE ROW LEVEL SECURITY";
        let p = parse_alter_table_rls_sql(sql).unwrap();
        assert!(matches!(p.action, AlterTableRlsAction::Force));
    }

    #[test]
    fn test_parse_alter_table_no_force_rls() {
        let sql = "ALTER TABLE users NO FORCE ROW LEVEL SECURITY";
        let p = parse_alter_table_rls_sql(sql).unwrap();
        assert!(matches!(p.action, AlterTableRlsAction::NoForce));
    }

    #[test]
    fn test_parse_alter_policy_using() {
        let sql = "ALTER POLICY my_policy ON users USING (role = 'admin')";
        let p = parse_alter_policy_sql(sql).unwrap();
        assert_eq!(p.name, "my_policy");
        assert_eq!(p.table, "users");
        assert!(p.roles.is_none());
        assert_eq!(
            p.using_expr.as_ref().unwrap().as_deref(),
            Some("role = 'admin'")
        );
        assert!(p.with_check_expr.is_none());
    }

    #[test]
    fn test_parse_alter_policy_roles_and_check() {
        let sql = "ALTER POLICY pol ON t TO admin, editor WITH CHECK (x > 0)";
        let p = parse_alter_policy_sql(sql).unwrap();
        assert_eq!(p.name, "pol");
        assert_eq!(p.table, "t");
        assert_eq!(p.roles.as_ref().unwrap(), &["admin", "editor"]);
        assert!(p.using_expr.is_none());
        assert_eq!(
            p.with_check_expr.as_ref().unwrap().as_deref(),
            Some("x > 0")
        );
    }

    #[test]
    fn test_parse_alter_policy_all_fields() {
        let sql = "ALTER POLICY pol ON t TO public USING (visible) WITH CHECK (x > 0)";
        let p = parse_alter_policy_sql(sql).unwrap();
        assert_eq!(p.roles.as_ref().unwrap(), &["public"]);
        assert_eq!(p.using_expr.as_ref().unwrap().as_deref(), Some("visible"));
        assert_eq!(
            p.with_check_expr.as_ref().unwrap().as_deref(),
            Some("x > 0")
        );
    }

    #[test]
    fn test_parse_alter_policy_no_changes_fails() {
        let sql = "ALTER POLICY pol ON t";
        assert!(parse_alter_policy_sql(sql).is_err());
    }

    #[test]
    fn test_parse_alter_policy_quoted_ident() {
        let sql = r#"ALTER POLICY "my policy" ON "my table" USING (x > 0)"#;
        let p = parse_alter_policy_sql(sql).unwrap();
        assert_eq!(p.name, "my policy");
        assert_eq!(p.table, "my table");
    }

    #[test]
    fn test_parse_create_policy_schema_qualified_table() {
        let sql = "CREATE POLICY pol ON public.users USING (true)";
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(p.table, "public.users");
    }

    #[test]
    fn test_parse_create_policy_nested_parens() {
        let sql = "CREATE POLICY pol ON t USING (x IN (SELECT id FROM other WHERE y = 1))";
        let p = parse_create_policy_sql(sql).unwrap();
        assert_eq!(
            p.using_expr.as_deref(),
            Some("x IN (SELECT id FROM other WHERE y = 1)")
        );
    }
}
