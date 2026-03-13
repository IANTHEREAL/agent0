use super::helpers::{text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::auth::{Privilege, PrivilegeObject, Role, User};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;
use tikv_client::BoundRange;

pub struct TablePrivileges;

const USER_KEY_PREFIX: &[u8] = b"_sys_user_";
const ROLE_KEY_PREFIX: &[u8] = b"_sys_role_";
const SCAN_LIMIT: u32 = u32::MAX;

fn privilege_type_strings(privilege: &Privilege) -> Vec<&'static str> {
    match privilege {
        Privilege::All => Privilege::expand_all()
            .into_iter()
            .filter_map(|p| privilege_type_strings(&p).into_iter().next())
            .collect(),
        Privilege::Select => vec!["SELECT"],
        Privilege::Insert => vec!["INSERT"],
        Privilege::Update => vec!["UPDATE"],
        Privilege::Delete => vec!["DELETE"],
        Privilege::Truncate => vec!["TRUNCATE"],
        Privilege::References => vec!["REFERENCES"],
        Privilege::Trigger => vec!["TRIGGER"],
        _ => Vec::new(),
    }
}

fn is_grantable_text(with_grant_option: bool) -> Value {
    text_val(if with_grant_option { "YES" } else { "NO" })
}

fn table_privilege_row(
    grantee: &str,
    table_catalog: &str,
    table_schema: &str,
    table_name: &str,
    privilege_type: &str,
    with_grant_option: bool,
) -> Row {
    // Follow PostgreSQL information_schema.table_privileges column order.
    Row::new(vec![
        text_val("postgres"),                 // grantor
        text_val(grantee),                    // grantee
        text_val(table_catalog),              // table_catalog
        text_val(table_schema),               // table_schema
        text_val(table_name),                 // table_name
        text_val(privilege_type),             // privilege_type
        is_grantable_text(with_grant_option), // is_grantable
        text_val("NO"),                       // with_hierarchy
    ])
}

#[async_trait]
impl VirtualTable for TablePrivileges {
    fn name(&self) -> &str {
        "table_privileges"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "table_privileges".to_string(),
            columns: vec![
                text_col("grantor"),
                text_col("grantee"),
                text_col("table_catalog"),
                text_col("table_schema"),
                text_col("table_name"),
                text_col("privilege_type"),
                text_col("is_grantable"),
                text_col("with_hierarchy"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut tables: std::collections::HashSet<String> =
            ctx.user_tables.iter().cloned().collect();
        let views = ctx.store.list_views(ctx.txn, ctx.db_id).await?;
        for view_def in views {
            tables.insert(format!("{}.{}", view_def.schema, view_def.name));
        }

        let mut rows = Vec::new();

        for (prefix, is_user) in [(USER_KEY_PREFIX, true), (ROLE_KEY_PREFIX, false)] {
            let prefix = prefix.to_vec();
            let mut end = prefix.clone();
            end.push(0xFF);
            let range: BoundRange = (prefix..end).into();
            let pairs = ctx.txn.scan(range, SCAN_LIMIT).await?;

            for pair in pairs {
                let privileges = if is_user {
                    let user: User = match bincode::deserialize(pair.value()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    (user.name, user.privileges)
                } else {
                    let role: Role = match bincode::deserialize(pair.value()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    (role.name, role.privileges)
                };

                let (grantee, granted) = privileges;
                for gp in granted {
                    let PrivilegeObject::Table { schema, name } = gp.object else {
                        continue;
                    };
                    let full = format!("{}.{}", schema, name);
                    if !tables.contains(&full) {
                        continue;
                    }

                    for priv_type in privilege_type_strings(&gp.privilege) {
                        rows.push(table_privilege_row(
                            &grantee,
                            ctx.database_name,
                            &schema,
                            &name,
                            priv_type,
                            gp.with_grant_option,
                        ));
                    }
                }
            }
        }

        // Keep output stable even without ORDER BY.
        rows.sort_by(|a, b| {
            let sa = match (&a.values[3], &a.values[4], &a.values[1], &a.values[5]) {
                (Value::Text(s1), Value::Text(s2), Value::Text(s3), Value::Text(s4)) => {
                    (s1.as_str(), s2.as_str(), s3.as_str(), s4.as_str())
                }
                _ => ("", "", "", ""),
            };
            let sb = match (&b.values[3], &b.values[4], &b.values[1], &b.values[5]) {
                (Value::Text(s1), Value::Text(s2), Value::Text(s3), Value::Text(s4)) => {
                    (s1.as_str(), s2.as_str(), s3.as_str(), s4.as_str())
                }
                _ => ("", "", "", ""),
            };
            sa.cmp(&sb)
        });

        // If there are no rows, return empty result set (schema still exists).
        Ok(rows)
    }
}
