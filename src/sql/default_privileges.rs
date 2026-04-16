use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tikv_client::{BoundRange, Transaction};

use crate::auth::{AuthManager, Privilege, PrivilegeObject};
use crate::txn::{txn_delete, txn_put};

use super::names;

const DEFAULT_TABLE_PRIV_PREFIX: &[u8] = b"_sys_default_table_priv_v1\0";
const SCAN_LIMIT: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultTablePrivilegeGrant {
    pub grantee: String,
    pub privilege: Privilege,
    pub with_grant_option: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterDefaultPrivilegesOp {
    Grant {
        privileges: Vec<Privilege>,
        grantees: Vec<String>,
        with_grant_option: bool,
    },
    Revoke {
        privileges: Vec<Privilege>,
        grantees: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterDefaultPrivilegesCommand {
    pub target_role: Option<String>,
    /// `None` means "all schemas" (no `IN SCHEMA`).
    pub schemas: Option<Vec<String>>,
    pub op: AlterDefaultPrivilegesOp,
}

fn default_table_priv_key(role: &str, db_id: u64, schema: Option<&str>) -> Vec<u8> {
    let schema = schema.unwrap_or("");
    let mut key = Vec::with_capacity(
        DEFAULT_TABLE_PRIV_PREFIX.len()
            + role.len()
            + 1
            + std::mem::size_of::<u64>()
            + 1
            + schema.len(),
    );
    key.extend_from_slice(DEFAULT_TABLE_PRIV_PREFIX);
    key.extend_from_slice(role.as_bytes());
    key.push(0);
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(0);
    key.extend_from_slice(schema.as_bytes());
    key
}

fn default_table_priv_prefix_for_scan() -> Vec<u8> {
    DEFAULT_TABLE_PRIV_PREFIX.to_vec()
}

fn default_table_priv_owner_from_key(key: &[u8]) -> Option<&[u8]> {
    let rest = key.strip_prefix(DEFAULT_TABLE_PRIV_PREFIX)?;
    let end = rest.iter().position(|b| *b == 0)?;
    Some(&rest[..end])
}

async fn put_default_table_privs(
    txn: &mut Transaction,
    key: Vec<u8>,
    grants: Vec<DefaultTablePrivilegeGrant>,
) -> Result<()> {
    if grants.is_empty() {
        txn_delete(txn, key).await?;
        return Ok(());
    }
    let data = bincode::serialize(&grants)?;
    txn_put(txn, key, data).await?;
    Ok(())
}

pub async fn get_default_table_privileges(
    txn: &mut Transaction,
    role: &str,
    db_id: u64,
    schema: Option<&str>,
) -> Result<Vec<DefaultTablePrivilegeGrant>> {
    let key = default_table_priv_key(role, db_id, schema);
    match txn.get(key).await? {
        Some(data) => Ok(bincode::deserialize(&data)?),
        None => Ok(Vec::new()),
    }
}

pub async fn apply_default_privileges_grant(
    txn: &mut Transaction,
    role: &str,
    db_id: u64,
    schema: Option<&str>,
    privileges: &[Privilege],
    grantees: &[String],
    with_grant_option: bool,
) -> Result<()> {
    let mut grants = get_default_table_privileges(txn, role, db_id, schema).await?;

    for grantee in grantees {
        for privilege in privileges {
            if let Some(existing) = grants
                .iter_mut()
                .find(|g| g.grantee == *grantee && g.privilege == *privilege)
            {
                existing.with_grant_option |= with_grant_option;
            } else {
                grants.push(DefaultTablePrivilegeGrant {
                    grantee: grantee.clone(),
                    privilege: privilege.clone(),
                    with_grant_option,
                });
            }
        }
    }

    let key = default_table_priv_key(role, db_id, schema);
    put_default_table_privs(txn, key, grants).await
}

pub async fn apply_default_privileges_revoke(
    txn: &mut Transaction,
    role: &str,
    db_id: u64,
    schema: Option<&str>,
    privileges: &[Privilege],
    grantees: &[String],
) -> Result<()> {
    let mut grants = get_default_table_privileges(txn, role, db_id, schema).await?;
    let revoke_all = privileges.contains(&Privilege::All);
    if revoke_all {
        grants.retain(|g| !grantees.contains(&g.grantee));
    } else {
        grants.retain(|g| !(grantees.contains(&g.grantee) && privileges.contains(&g.privilege)));
    }

    let key = default_table_priv_key(role, db_id, schema);
    put_default_table_privs(txn, key, grants).await
}

pub async fn apply_default_table_privileges_for_new_table(
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    db_id: u64,
    owner_role: &str,
    table_full_name: &str,
) -> Result<()> {
    let (table_schema, table_name) = names::parse_full_name(table_full_name)
        .unwrap_or(("public".to_string(), table_full_name.to_string()));

    let schema_grants =
        get_default_table_privileges(txn, owner_role, db_id, Some(&table_schema)).await?;
    let global_grants = get_default_table_privileges(txn, owner_role, db_id, None).await?;

    // Merge by (grantee, privilege), OR-ing grant option.
    let mut merged: std::collections::HashMap<(String, Privilege), bool> =
        std::collections::HashMap::new();
    for g in schema_grants.into_iter().chain(global_grants) {
        let entry = merged.entry((g.grantee, g.privilege)).or_insert(false);
        *entry |= g.with_grant_option;
    }

    for ((grantee, privilege), with_grant_option) in merged {
        let mut expanded: Vec<Privilege> = Vec::new();
        if privilege == Privilege::All {
            expanded.extend(Privilege::expand_all());
        } else {
            expanded.push(privilege);
        }

        for privilege in expanded {
            grant_table_privilege(
                auth_manager,
                txn,
                &grantee,
                privilege,
                &table_schema,
                &table_name,
                with_grant_option,
            )
            .await?;
        }
    }

    Ok(())
}

async fn grant_table_privilege(
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    grantee: &str,
    privilege: Privilege,
    table_schema: &str,
    table_name: &str,
    with_grant_option: bool,
) -> Result<()> {
    let object = PrivilegeObject::Table {
        schema: table_schema.to_string(),
        name: table_name.to_string(),
    };

    if let Some(mut user) = auth_manager.get_user(txn, grantee).await? {
        user.grant_privilege(privilege, object, with_grant_option);
        auth_manager.update_user(txn, user).await?;
        return Ok(());
    }

    if let Some(mut role) = auth_manager.get_role(txn, grantee).await? {
        role.privileges
            .retain(|p| !(p.privilege == privilege && p.object == object));
        role.privileges.push(crate::auth::GrantedPrivilege {
            privilege,
            object,
            with_grant_option,
        });
        auth_manager.update_role(txn, role).await?;
        return Ok(());
    }

    // Best-effort: ignore missing grantee to avoid breaking CREATE TABLE.
    Ok(())
}

/// Remove default privilege entries owned by `role` and drop any grants that
/// reference `role` as a grantee.
pub async fn cleanup_default_table_privileges_for_role(
    txn: &mut Transaction,
    role: &str,
) -> Result<()> {
    let prefix = default_table_priv_prefix_for_scan();
    let mut end = prefix.clone();
    end.push(0xFF);
    let range: BoundRange = (prefix.clone()..end).into();
    let pairs = txn.scan(range, SCAN_LIMIT).await?;

    for pair in pairs {
        let key: Vec<u8> = pair.key().to_owned().into();
        let value = pair.value();

        let owner = default_table_priv_owner_from_key(&key)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("");
        if owner == role {
            txn_delete(txn, key).await?;
            continue;
        }

        let mut grants: Vec<DefaultTablePrivilegeGrant> = match bincode::deserialize(value) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let before = grants.len();
        grants.retain(|g| g.grantee != role);
        if grants.len() != before {
            put_default_table_privs(txn, key, grants).await?;
        }
    }

    Ok(())
}

fn parse_identifier_token(token: &str) -> Result<String> {
    let token = token.trim();
    if token.is_empty() {
        return Err(anyhow!("missing identifier"));
    }
    if token.starts_with('"') {
        if token.len() < 2 || !token.ends_with('"') {
            return Err(anyhow!("invalid quoted identifier"));
        }
        let inner = &token[1..token.len() - 1];
        Ok(inner.replace("\"\"", "\""))
    } else {
        Ok(token.to_string())
    }
}

fn tokenize_sql(input: &str) -> Vec<&str> {
    use crate::sql::scanner::SqlCharScanner;

    let mut tokens = Vec::new();
    let mut token_start: Option<usize> = None;
    let mut prev_in_string = false;

    for ctx in SqlCharScanner::new(input) {
        if ctx.in_string() {
            if token_start.is_none() {
                token_start = Some(ctx.pos);
            }
            prev_in_string = true;
            continue;
        }

        // Just exited a string — emit it as a token.
        if prev_in_string {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            prev_in_string = false;
        }

        let b = ctx.byte;
        if b.is_ascii_whitespace() {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            continue;
        }

        if matches!(b, b',' | b';') {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            tokens.push(&input[ctx.pos..ctx.pos + 1]);
            continue;
        }

        if token_start.is_none() {
            token_start = Some(ctx.pos);
        }
    }

    // Flush remaining token.
    if let Some(start) = token_start {
        tokens.push(&input[start..]);
    }

    tokens
}

fn strip_trailing_semicolons(tokens: &mut Vec<&str>) {
    while matches!(tokens.last().copied(), Some(";")) {
        tokens.pop();
    }
}

pub fn parse_alter_default_privileges_sql(sql: &str) -> Result<AlterDefaultPrivilegesCommand> {
    let sql = super::executor::triggers::strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid ALTER DEFAULT PRIVILEGES syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "ALTER")?;
    expect(&tokens, &mut pos, "DEFAULT")?;
    expect(&tokens, &mut pos, "PRIVILEGES")?;

    let mut target_role: Option<String> = None;
    if tokens
        .get(pos)
        .copied()
        .unwrap_or("")
        .eq_ignore_ascii_case("FOR")
    {
        pos += 1;
        let kind = tokens.get(pos).copied().unwrap_or("");
        if !kind.eq_ignore_ascii_case("ROLE") && !kind.eq_ignore_ascii_case("USER") {
            return Err(anyhow!("Invalid ALTER DEFAULT PRIVILEGES syntax"));
        }
        pos += 1;
        let role_tok = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing role name"))?;
        pos += 1;
        target_role = Some(parse_identifier_token(role_tok)?);
    }

    let mut schemas: Option<Vec<String>> = None;
    if tokens
        .get(pos)
        .copied()
        .unwrap_or("")
        .eq_ignore_ascii_case("IN")
        && tokens
            .get(pos + 1)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("SCHEMA")
    {
        pos += 2;
        let mut out = Vec::new();
        loop {
            let schema_tok = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing schema name"))?;
            pos += 1;
            out.push(parse_identifier_token(schema_tok)?);
            if tokens.get(pos).copied() == Some(",") {
                pos += 1;
                continue;
            }
            break;
        }
        schemas = Some(out);
    }

    let op_tok = tokens.get(pos).copied().unwrap_or("");
    let is_grant = op_tok.eq_ignore_ascii_case("GRANT");
    let is_revoke = op_tok.eq_ignore_ascii_case("REVOKE");
    if !is_grant && !is_revoke {
        return Err(anyhow!("Invalid ALTER DEFAULT PRIVILEGES syntax"));
    }
    pos += 1;

    let mut privileges: Vec<Privilege> = Vec::new();
    if tokens
        .get(pos)
        .copied()
        .unwrap_or("")
        .eq_ignore_ascii_case("ALL")
    {
        privileges.push(Privilege::All);
        pos += 1;
        if tokens
            .get(pos)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("PRIVILEGES")
        {
            pos += 1;
        }
    } else {
        loop {
            let priv_tok = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing privilege"))?;
            pos += 1;
            let priv_name = parse_identifier_token(priv_tok)?;
            let p = Privilege::from_str(&priv_name)
                .ok_or_else(|| anyhow!("Unsupported privilege '{}'", priv_name))?;
            privileges.push(p);
            if tokens.get(pos).copied() == Some(",") {
                pos += 1;
                continue;
            }
            break;
        }
    }

    expect(&tokens, &mut pos, "ON")?;
    expect(&tokens, &mut pos, "TABLES")?;

    if is_grant {
        expect(&tokens, &mut pos, "TO")?;
        let mut grantees = Vec::new();
        loop {
            let tok = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing grantee"))?;
            pos += 1;
            grantees.push(parse_identifier_token(tok)?);
            if tokens.get(pos).copied() == Some(",") {
                pos += 1;
                continue;
            }
            break;
        }

        let mut with_grant_option = false;
        if tokens
            .get(pos)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("WITH")
        {
            pos += 1;
            expect(&tokens, &mut pos, "GRANT")?;
            expect(&tokens, &mut pos, "OPTION")?;
            with_grant_option = true;
        }

        if pos < tokens.len() {
            return Err(anyhow!("Unsupported ALTER DEFAULT PRIVILEGES syntax"));
        }

        return Ok(AlterDefaultPrivilegesCommand {
            target_role,
            schemas,
            op: AlterDefaultPrivilegesOp::Grant {
                privileges,
                grantees,
                with_grant_option,
            },
        });
    }

    // REVOKE
    expect(&tokens, &mut pos, "FROM")?;
    let mut grantees = Vec::new();
    loop {
        let tok = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing grantee"))?;
        pos += 1;
        grantees.push(parse_identifier_token(tok)?);
        if tokens.get(pos).copied() == Some(",") {
            pos += 1;
            continue;
        }
        break;
    }

    if pos < tokens.len() {
        return Err(anyhow!("Unsupported ALTER DEFAULT PRIVILEGES syntax"));
    }

    Ok(AlterDefaultPrivilegesCommand {
        target_role,
        schemas,
        op: AlterDefaultPrivilegesOp::Revoke {
            privileges,
            grantees,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alter_default_privileges_grant_schema() {
        let cmd = parse_alter_default_privileges_sql(
            "ALTER DEFAULT PRIVILEGES FOR ROLE adp_owner IN SCHEMA adp_schema GRANT SELECT ON TABLES TO adp_grantee;",
        )
        .unwrap();
        assert_eq!(cmd.target_role.as_deref(), Some("adp_owner"));
        let expected = vec!["adp_schema".to_string()];
        assert_eq!(cmd.schemas.as_deref(), Some(expected.as_slice()));
        match cmd.op {
            AlterDefaultPrivilegesOp::Grant {
                privileges,
                grantees,
                with_grant_option,
            } => {
                assert_eq!(privileges, vec![Privilege::Select]);
                assert_eq!(grantees, vec!["adp_grantee".to_string()]);
                assert!(!with_grant_option);
            }
            _ => panic!("expected grant"),
        }
    }

    #[test]
    fn parse_alter_default_privileges_revoke_global() {
        let cmd = parse_alter_default_privileges_sql(
            "ALTER DEFAULT PRIVILEGES FOR ROLE adp_owner REVOKE SELECT ON TABLES FROM adp_grantee;",
        )
        .unwrap();
        assert_eq!(cmd.schemas, None);
        match cmd.op {
            AlterDefaultPrivilegesOp::Revoke {
                privileges,
                grantees,
            } => {
                assert_eq!(privileges, vec![Privilege::Select]);
                assert_eq!(grantees, vec!["adp_grantee".to_string()]);
            }
            _ => panic!("expected revoke"),
        }
    }
}
