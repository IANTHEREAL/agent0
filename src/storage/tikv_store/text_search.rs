//! Per-tenant text search configuration persistence.
//!
//! Stores `config_name → tokenizer_name` mappings in TiKV, keyed by
//! `d_{db_id}_sys_tsc_{config_name}`.  The value is the UTF-8 tokenizer
//! name (e.g. `"zhparser"`, `"jieba"`).

use super::*;

impl TikvStore {
    /// Look up a user-defined text search configuration.
    ///
    /// Returns the tokenizer name (e.g. `"zhparser"`) if the config exists.
    pub async fn get_text_search_config(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        config_name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_tsc_key_v2(db_id, config_name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                String::from_utf8(data).context("invalid UTF-8 in text search config value")?,
            )),
            None => Ok(None),
        }
    }

    /// Persist a user-defined text search configuration.
    pub async fn put_text_search_config(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        config_name: &str,
        tokenizer_name: &str,
    ) -> Result<()> {
        let key = self.key(&encode_tsc_key_v2(db_id, config_name));
        txn_put(txn, key, tokenizer_name.as_bytes().to_vec()).await?;
        Ok(())
    }

    /// Drop a user-defined text search configuration.
    ///
    /// Returns `true` if the config existed and was deleted.
    pub async fn drop_text_search_config(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        config_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_tsc_key_v2(db_id, config_name));
        let existed = tikv_op!(txn.get(key.clone()).await)?.is_some();
        if existed {
            txn_delete(txn, key).await?;
        }
        Ok(existed)
    }
}
