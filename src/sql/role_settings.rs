use std::str;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tikv_client::{BoundRange, Transaction};

use crate::txn::{txn_delete, txn_put};

const DB_ROLE_SETTING_KEY_PREFIX: &[u8] = b"_sys_db_role_setting_";
const ROLE_SEPARATOR: u8 = 0;
const SCAN_LIMIT: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DbRoleSetting {
    pub(crate) role_name: String,
    pub(crate) database_oid: u32,
    pub(crate) setconfig: Vec<String>,
}

fn key_for(role_name: &str, database_oid: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(
        DB_ROLE_SETTING_KEY_PREFIX.len() + role_name.len() + 1 + std::mem::size_of::<u32>(),
    );
    key.extend_from_slice(DB_ROLE_SETTING_KEY_PREFIX);
    key.extend_from_slice(role_name.as_bytes());
    key.push(ROLE_SEPARATOR);
    key.extend_from_slice(&database_oid.to_be_bytes());
    key
}

fn role_prefix(role_name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(DB_ROLE_SETTING_KEY_PREFIX.len() + role_name.len() + 1);
    key.extend_from_slice(DB_ROLE_SETTING_KEY_PREFIX);
    key.extend_from_slice(role_name.as_bytes());
    key.push(ROLE_SEPARATOR);
    key
}

fn decode_key(key: &[u8]) -> Result<(String, u32)> {
    if !key.starts_with(DB_ROLE_SETTING_KEY_PREFIX) {
        return Err(anyhow!("invalid db role setting key prefix"));
    }

    let rest = &key[DB_ROLE_SETTING_KEY_PREFIX.len()..];
    let Some(sep_pos) = rest.iter().position(|b| *b == ROLE_SEPARATOR) else {
        return Err(anyhow!("invalid db role setting key separator"));
    };

    let role_bytes = &rest[..sep_pos];
    if role_bytes.is_empty() {
        return Err(anyhow!("invalid db role setting role name"));
    }

    let db_bytes = &rest[(sep_pos + 1)..];
    let db_bytes: [u8; 4] = db_bytes
        .try_into()
        .map_err(|_| anyhow!("invalid db role setting database oid"))?;

    let role_name = str::from_utf8(role_bytes)
        .map_err(|_| anyhow!("invalid db role setting role name encoding"))?
        .to_string();

    Ok((role_name, u32::from_be_bytes(db_bytes)))
}

fn normalize_setting_name(setting_name: &str) -> String {
    setting_name.trim().to_ascii_lowercase()
}

fn remove_setting(setconfig: &mut Vec<String>, setting_name: &str) {
    let setting_name = normalize_setting_name(setting_name);
    if setting_name.is_empty() {
        return;
    }

    let prefix = format!("{setting_name}=");
    setconfig.retain(|cfg| !cfg.to_ascii_lowercase().starts_with(&prefix));
}

async fn load_setconfig(
    txn: &mut Transaction,
    role_name: &str,
    database_oid: u32,
) -> Result<Vec<String>> {
    let key = key_for(role_name, database_oid);
    match txn.get(key).await? {
        Some(data) => Ok(bincode::deserialize(&data)?),
        None => Ok(Vec::new()),
    }
}

async fn store_setconfig(
    txn: &mut Transaction,
    role_name: &str,
    database_oid: u32,
    mut setconfig: Vec<String>,
) -> Result<()> {
    setconfig.sort();
    setconfig.dedup();
    let key = key_for(role_name, database_oid);
    if setconfig.is_empty() {
        txn_delete(txn, key).await?;
        return Ok(());
    }
    let data = bincode::serialize(&setconfig)?;
    txn_put(txn, key, data).await?;
    Ok(())
}

pub(crate) async fn set_role_setting(
    txn: &mut Transaction,
    role_name: &str,
    database_oid: u32,
    setting_name: &str,
    setting_value: &str,
) -> Result<()> {
    let setting_name = normalize_setting_name(setting_name);
    if setting_name.is_empty() {
        return Err(anyhow!("empty role setting name"));
    }
    let setting_value = setting_value.trim();

    let mut setconfig = load_setconfig(txn, role_name, database_oid).await?;
    remove_setting(&mut setconfig, &setting_name);
    setconfig.push(format!("{setting_name}={setting_value}"));
    store_setconfig(txn, role_name, database_oid, setconfig).await
}

pub(crate) async fn reset_role_setting(
    txn: &mut Transaction,
    role_name: &str,
    database_oid: u32,
    setting_name: &str,
) -> Result<()> {
    let setting_name = normalize_setting_name(setting_name);
    if setting_name.is_empty() {
        return Ok(());
    }

    let mut setconfig = load_setconfig(txn, role_name, database_oid).await?;
    remove_setting(&mut setconfig, &setting_name);
    store_setconfig(txn, role_name, database_oid, setconfig).await
}

pub(crate) async fn reset_role_settings_all(
    txn: &mut Transaction,
    role_name: &str,
    database_oid: u32,
) -> Result<()> {
    let key = key_for(role_name, database_oid);
    txn_delete(txn, key).await?;
    Ok(())
}

pub(crate) async fn delete_role_settings_for_role(
    txn: &mut Transaction,
    role_name: &str,
) -> Result<()> {
    let prefix = role_prefix(role_name);
    let mut end = prefix.clone();
    end.push(0xFF);

    let range: BoundRange = (prefix..end).into();
    let pairs = txn.scan(range, SCAN_LIMIT).await?;
    for pair in pairs {
        let key: &[u8] = pair.key().as_ref().into();
        txn_delete(txn, key.to_vec()).await?;
    }
    Ok(())
}

pub(crate) async fn rename_role_settings(
    txn: &mut Transaction,
    old_role_name: &str,
    new_role_name: &str,
) -> Result<()> {
    let prefix = role_prefix(old_role_name);
    let mut end = prefix.clone();
    end.push(0xFF);

    let range: BoundRange = (prefix..end).into();
    let pairs = txn.scan(range, SCAN_LIMIT).await?;

    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut old_keys: Vec<Vec<u8>> = Vec::new();
    for pair in pairs {
        let key: &[u8] = pair.key().as_ref().into();
        let (_role_name, database_oid) = decode_key(key)?;
        entries.push((database_oid, pair.value().to_vec()));
        old_keys.push(key.to_vec());
    }

    for (database_oid, value) in entries {
        let new_key = key_for(new_role_name, database_oid);
        txn_put(txn, new_key, value).await?;
    }
    for old_key in old_keys {
        txn_delete(txn, old_key).await?;
    }

    Ok(())
}

pub(crate) async fn list_db_role_settings(txn: &mut Transaction) -> Result<Vec<DbRoleSetting>> {
    let prefix = DB_ROLE_SETTING_KEY_PREFIX.to_vec();
    let mut end = prefix.clone();
    end.push(0xFF);

    let range: BoundRange = (prefix..end).into();
    let pairs = txn.scan(range, SCAN_LIMIT).await?;

    let mut out = Vec::new();
    for pair in pairs {
        let key: &[u8] = pair.key().as_ref().into();
        if !key.starts_with(DB_ROLE_SETTING_KEY_PREFIX) {
            continue;
        }
        let (role_name, database_oid) = decode_key(key)?;
        let mut setconfig: Vec<String> = bincode::deserialize(pair.value())?;
        setconfig.sort();
        out.push(DbRoleSetting {
            role_name,
            database_oid,
            setconfig,
        });
    }

    out.sort_by(|a, b| match a.role_name.cmp(&b.role_name) {
        std::cmp::Ordering::Equal => a.database_oid.cmp(&b.database_oid),
        other => other,
    });
    Ok(out)
}
