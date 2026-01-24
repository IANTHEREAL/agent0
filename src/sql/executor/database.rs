use super::core::Executor;
use super::triggers::strip_leading_sql_comments;
use super::super::{ExecuteResult, ExecuteResults, Session};
use anyhow::{anyhow, Result};
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateDatabaseCommand {
    name: String,
    if_not_exists: bool,
    owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DropDatabaseCommand {
    name: String,
    if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlterDatabaseCommand {
    Rename { old_name: String, new_name: String },
    Owner { name: String, new_owner: String },
}

fn is_reserved_database_name(name: &str) -> bool {
    matches!(name, "postgres" | "template0" | "template1")
}

fn validate_database_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        return Err(anyhow!("invalid database name: {}", name));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(anyhow!("invalid database name: {}", name));
    }
    Ok(())
}

fn validate_database_name_for_create(name: &str) -> Result<()> {
    validate_database_name(name)?;
    if is_reserved_database_name(name) {
        return Err(anyhow!("cannot use reserved database name \"{}\"", name));
    }
    Ok(())
}

fn parse_single_quoted_literal(token: &str) -> Result<String> {
    if token.len() < 2 || !token.starts_with('\'') || !token.ends_with('\'') {
        return Err(anyhow!("invalid string literal"));
    }
    let inner = &token[1..token.len() - 1];
    // PostgreSQL escapes single quotes as doubled quotes inside string literals.
    Ok(inner.replace("''", "'"))
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

fn parse_value_as_string(token: &str) -> Result<String> {
    if token.starts_with('\'') {
        parse_single_quoted_literal(token)
    } else {
        parse_identifier_token(token)
    }
}

fn tokenize_sql(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let start = i;
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            // Escaped quote: consume both.
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                tokens.push(&input[start..i]);
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            // Escaped quote: consume both.
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                tokens.push(&input[start..i]);
            }
            b'(' | b')' | b',' | b';' | b'=' => {
                i += 1;
                tokens.push(&input[start..i]);
            }
            _ => {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b.is_ascii_whitespace() || matches!(b, b'(' | b')' | b',' | b';' | b'=') {
                        break;
                    }
                    i += 1;
                }
                tokens.push(&input[start..i]);
            }
        }
    }

    tokens
}

fn strip_trailing_semicolons(tokens: &mut Vec<&str>) {
    while matches!(tokens.last().copied(), Some(";")) {
        tokens.pop();
    }
}

fn parse_create_database_sql(sql: &str) -> Result<CreateDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid CREATE DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "CREATE")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let mut if_not_exists = false;
    if tokens.get(pos).copied().unwrap_or("").eq_ignore_ascii_case("IF")
        && tokens
            .get(pos + 1)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("NOT")
        && tokens
            .get(pos + 2)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("EXISTS")
    {
        if_not_exists = true;
        pos += 3;
    }

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name_for_create(&name)?;

    let mut owner: Option<String> = None;

    while pos < tokens.len() {
        let tok = tokens[pos];
        if tok.eq_ignore_ascii_case("WITH") {
            pos += 1;
            continue;
        }

        if tok.eq_ignore_ascii_case("OWNER") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let owner_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing owner name"))?;
            pos += 1;
            owner = Some(parse_identifier_token(owner_token)?.to_ascii_lowercase());
            continue;
        }

        if tok.eq_ignore_ascii_case("TEMPLATE") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let tmpl_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing TEMPLATE value"))?;
            pos += 1;
            let tmpl = parse_identifier_token(tmpl_token)?.to_ascii_lowercase();
            if !matches!(tmpl.as_str(), "template0" | "template1") {
                return Err(anyhow!(
                    "CREATE DATABASE TEMPLATE is not supported (got \"{}\")",
                    tmpl
                ));
            }
            continue;
        }

        if tok.eq_ignore_ascii_case("ENCODING") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let enc_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing ENCODING value"))?;
            pos += 1;
            let enc = parse_value_as_string(enc_token)?.to_ascii_lowercase();
            if enc != "utf8" && enc != "utf-8" {
                return Err(anyhow!("only UTF8 encoding is supported"));
            }
            continue;
        }

        if tok.eq_ignore_ascii_case("LC_COLLATE")
            || tok.eq_ignore_ascii_case("LC_CTYPE")
            || tok.eq_ignore_ascii_case("LOCALE")
        {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            // Accept and ignore locale values from pg_dump/pg_restore scripts.
            let _ = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing {} value", tok))?;
            pos += 1;
            continue;
        }

        return Err(anyhow!("Unsupported CREATE DATABASE option: {}", tok));
    }

    Ok(CreateDatabaseCommand {
        name,
        if_not_exists,
        owner,
    })
}

fn parse_drop_database_sql(sql: &str) -> Result<DropDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid DROP DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "DROP")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let mut if_exists = false;
    if tokens.get(pos).copied().unwrap_or("").eq_ignore_ascii_case("IF")
        && tokens
            .get(pos + 1)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("EXISTS")
    {
        if_exists = true;
        pos += 2;
    }

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name(&name)?;

    if pos < tokens.len() {
        return Err(anyhow!("Unsupported DROP DATABASE syntax"));
    }

    Ok(DropDatabaseCommand { name, if_exists })
}

fn parse_alter_database_sql(sql: &str) -> Result<AlterDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid ALTER DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "ALTER")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name(&name)?;

    let op = tokens.get(pos).copied().unwrap_or("");
    if op.eq_ignore_ascii_case("RENAME") {
        pos += 1;
        expect(&tokens, &mut pos, "TO")?;
        let new_token = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing new database name"))?;
        pos += 1;
        let new_name = parse_identifier_token(new_token)?.to_ascii_lowercase();
        validate_database_name(&new_name)?;
        if pos < tokens.len() {
            return Err(anyhow!("Unsupported ALTER DATABASE syntax"));
        }
        return Ok(AlterDatabaseCommand::Rename {
            old_name: name,
            new_name,
        });
    }

    if op.eq_ignore_ascii_case("OWNER") {
        pos += 1;
        if tokens.get(pos).copied().unwrap_or("").eq_ignore_ascii_case("TO") {
            pos += 1;
        } else if tokens.get(pos).copied() == Some("=") {
            pos += 1;
        }
        let owner_token = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing owner name"))?;
        pos += 1;
        let new_owner = parse_identifier_token(owner_token)?.to_ascii_lowercase();
        if pos < tokens.len() {
            return Err(anyhow!("Unsupported ALTER DATABASE syntax"));
        }
        return Ok(AlterDatabaseCommand::Owner {
            name,
            new_owner,
        });
    }

    Err(anyhow!("Unsupported ALTER DATABASE operation"))
}

impl Executor {
    pub(crate) async fn execute_create_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_create_database_sql(sql)?;

        if !session.is_superuser() {
            return Err(anyhow!("permission denied to create database"));
        }
        if session.is_in_transaction() {
            return Err(anyhow!("CREATE DATABASE cannot run inside a transaction block"));
        }

        session.begin().await?;
        let result: Result<Vec<ExecuteResult>> = async {
            let owner = cmd
                .owner
                .clone()
                .or_else(|| session.current_user().map(|u| u.to_string()))
                .unwrap_or_else(|| "postgres".to_string());

            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let created = self
                .store()
                .create_database(txn, &cmd.name, &owner, cmd.if_not_exists)
                .await?;

            let mut results = Vec::new();
            if created.is_none() {
                results.push(ExecuteResult::Notice {
                    message: format!("database \"{}\" already exists, skipping", cmd.name),
                });
            }
            results.push(ExecuteResult::CommandComplete {
                tag: "CREATE DATABASE",
            });
            Ok(results)
        }
        .await;

        if result.is_ok() {
            session.commit().await?;
        } else {
            session.rollback().await?;
        }

        Ok(ExecuteResults(result?))
    }

    pub(crate) async fn execute_drop_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_drop_database_sql(sql)?;

        if !session.is_superuser() {
            return Err(anyhow!("permission denied to drop database"));
        }
        if session.is_in_transaction() {
            return Err(anyhow!("DROP DATABASE cannot run inside a transaction block"));
        }

        session.begin().await?;
        let result: Result<(Option<u64>, Vec<ExecuteResult>)> = async {
            let current_db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let dropped = self
                .store()
                .drop_database_metadata(txn, &cmd.name, cmd.if_exists, current_db_id)
                .await?;

            let mut results = Vec::new();
            if dropped.is_none() && cmd.if_exists {
                results.push(ExecuteResult::Notice {
                    message: format!("database \"{}\" does not exist, skipping", cmd.name),
                });
            }
            Ok((dropped, results))
        }
        .await;

        if result.is_ok() {
            session.commit().await?;
        } else {
            session.rollback().await?;
        }

        let (dropped_db_id, mut results) = result?;
        if let Some(db_id) = dropped_db_id {
            if let Err(e) = self.store().unsafe_destroy_database_data(db_id).await {
                warn!(
                    "DROP DATABASE '{}': failed to destroy data range for db_id={}: {}",
                    cmd.name, db_id, e
                );
            }
        }

        results.push(ExecuteResult::CommandComplete { tag: "DROP DATABASE" });
        Ok(ExecuteResults(results))
    }

    pub(crate) async fn execute_alter_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_alter_database_sql(sql)?;

        if !session.is_superuser() {
            return Err(anyhow!("permission denied to alter database"));
        }
        if session.is_in_transaction() {
            return Err(anyhow!("ALTER DATABASE cannot run inside a transaction block"));
        }

        session.begin().await?;
        let result: Result<()> = async {
            let current_db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match cmd {
                AlterDatabaseCommand::Rename { old_name, new_name } => {
                    self.store()
                        .rename_database(txn, &old_name, &new_name, current_db_id)
                        .await?;
                }
                AlterDatabaseCommand::Owner { name, new_owner } => {
                    self.store()
                        .set_database_owner(txn, &name, &new_owner)
                        .await?;
                }
            }
            Ok(())
        }
        .await;

        if result.is_ok() {
            session.commit().await?;
        } else {
            session.rollback().await?;
        }

        result?;
        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "ALTER DATABASE",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_respects_single_quoted_strings_with_spaces() {
        let tokens = tokenize_sql("LC_COLLATE = 'English_United States.1252';");
        assert_eq!(
            tokens,
            vec!["LC_COLLATE", "=", "'English_United States.1252'", ";"]
        );
    }

    #[test]
    fn test_parse_create_database_minimal() {
        let cmd = parse_create_database_sql("CREATE DATABASE testdb;").unwrap();
        assert_eq!(
            cmd,
            CreateDatabaseCommand {
                name: "testdb".to_string(),
                if_not_exists: false,
                owner: None
            }
        );
    }

    #[test]
    fn test_parse_create_database_if_not_exists_and_owner() {
        let cmd =
            parse_create_database_sql("CREATE DATABASE IF NOT EXISTS testdb WITH OWNER = admin;")
                .unwrap();
        assert_eq!(cmd.name, "testdb");
        assert!(cmd.if_not_exists);
        assert_eq!(cmd.owner.as_deref(), Some("admin"));
    }

    #[test]
    fn test_parse_create_database_pg_dump_style_options() {
        let cmd = parse_create_database_sql(
            "CREATE DATABASE dvdrental WITH TEMPLATE = template0 ENCODING = 'UTF8' LC_COLLATE = 'English_United States.1252' LC_CTYPE = 'English_United States.1252';",
        )
        .unwrap();
        assert_eq!(cmd.name, "dvdrental");
        assert_eq!(cmd.owner, None);
    }

    #[test]
    fn test_parse_drop_database_if_exists() {
        let cmd = parse_drop_database_sql("DROP DATABASE IF EXISTS testdb;").unwrap();
        assert_eq!(
            cmd,
            DropDatabaseCommand {
                name: "testdb".to_string(),
                if_exists: true
            }
        );
    }

    #[test]
    fn test_parse_alter_database_rename() {
        let cmd = parse_alter_database_sql("ALTER DATABASE testdb RENAME TO newdb;").unwrap();
        assert_eq!(
            cmd,
            AlterDatabaseCommand::Rename {
                old_name: "testdb".to_string(),
                new_name: "newdb".to_string()
            }
        );
    }

    #[test]
    fn test_parse_alter_database_owner_to() {
        let cmd = parse_alter_database_sql("ALTER DATABASE testdb OWNER TO admin;").unwrap();
        assert_eq!(
            cmd,
            AlterDatabaseCommand::Owner {
                name: "testdb".to_string(),
                new_owner: "admin".to_string()
            }
        );
    }
}
