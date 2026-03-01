use super::*;
use crate::sql::error::SqlError;
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    pub async fn create_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        owner: &str,
        query: &str,
        deps: Vec<String>,
        relation_bindings: Vec<String>,
        or_replace: bool,
    ) -> Result<()> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        let bindings_key = self.key(&encode_view_bindings_key_v2(db_id, name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            if !or_replace {
                return Err(SqlError::DuplicateRelation(name.to_string()).into());
            }

            let mut def = self
                .get_view(txn, db_id, name)
                .await?
                .ok_or_else(|| anyhow!("View '{}' does not exist", name))?;
            def.query = query.to_string();
            def.deps = deps;
            let data = bincode::serialize(&def).context("Failed to serialize view definition")?;
            txn_put(txn, key, data).await?;
            let bindings = bincode::serialize(&relation_bindings)
                .context("Failed to serialize view relation bindings")?;
            txn_put(txn, bindings_key, bindings).await?;
            info!("Replaced view '{}'", name);
            return Ok(());
        }

        let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
        let oid = self.next_view_oid(txn, db_id).await?;
        let def = ViewDef {
            oid,
            schema: schema.to_string(),
            name: view_name.to_string(),
            owner: owner.to_string(),
            query: query.to_string(),
            deps,
        };
        let data = bincode::serialize(&def).context("Failed to serialize view definition")?;
        txn_put(txn, key, data).await?;
        let bindings = bincode::serialize(&relation_bindings)
            .context("Failed to serialize view relation bindings")?;
        txn_put(txn, bindings_key, bindings).await?;
        info!("Created view '{}'", name);
        Ok(())
    }

    pub async fn get_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<ViewDef>> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => {
                let def: ViewDef =
                    bincode::deserialize(&data).context("Failed to deserialize view definition")?;
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    /// Update only the stored SQL text for an existing view definition.
    pub async fn update_view_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
    ) -> Result<()> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        let mut def = self
            .get_view(txn, db_id, name)
            .await?
            .ok_or_else(|| anyhow!("View '{}' does not exist", name))?;
        def.query = query.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize view definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_view(&self, txn: &mut Transaction, db_id: u64, name: &str) -> Result<bool> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        let bindings_key = self.key(&encode_view_bindings_key_v2(db_id, name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
            txn_delete(txn, bindings_key).await?;
            info!("Dropped view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_views(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<ViewDef>> {
        let prefix = encode_view_prefix_v2(db_id);
        let bindings_prefix = encode_view_bindings_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;
        let mut views = Vec::new();
        for pair in pairs {
            let key_bytes: &[u8] = pair.key().as_ref().into();
            if !is_definition_key(key_bytes, &prefix, &bindings_prefix) {
                continue;
            }
            let def: ViewDef = bincode::deserialize(pair.value())
                .context("Failed to deserialize view definition")?;
            views.push(def);
        }
        Ok(views)
    }

    pub async fn get_view_relation_bindings(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<Vec<String>>> {
        let key = self.key(&encode_view_bindings_key_v2(db_id, name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => {
                let bindings: Vec<String> = bincode::deserialize(&data)
                    .context("Failed to deserialize view relation bindings")?;
                Ok(Some(bindings))
            }
            None => Ok(None),
        }
    }

    pub async fn set_view_relation_bindings(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        relation_bindings: Vec<String>,
    ) -> Result<()> {
        let key = self.key(&encode_view_bindings_key_v2(db_id, name));
        let bindings = bincode::serialize(&relation_bindings)
            .context("Failed to serialize view relation bindings")?;
        txn_put(txn, key, bindings).await?;
        Ok(())
    }

    pub async fn create_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
        deps: Vec<String>,
        relation_bindings: Vec<String>,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        let bindings_key = self.key(&encode_matview_bindings_key_v2(db_id, name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Err(SqlError::DuplicateRelation(name.to_string()).into());
        }
        let (schema, mv_name) = name.split_once('.').unwrap_or(("public", name));
        let def = MatViewDef {
            schema: schema.to_string(),
            name: mv_name.to_string(),
            query: query.to_string(),
            deps,
        };
        let data = bincode::serialize(&def).context("Failed to serialize matview definition")?;
        txn_put(txn, key, data).await?;
        let bindings = bincode::serialize(&relation_bindings)
            .context("Failed to serialize matview relation bindings")?;
        txn_put(txn, bindings_key, bindings).await?;
        info!("Created materialized view '{}'", name);
        Ok(())
    }

    pub async fn get_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<MatViewDef>> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => {
                let def: MatViewDef = bincode::deserialize(&data)
                    .context("Failed to deserialize matview definition")?;
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    /// Update only the stored SQL text for an existing materialized view definition.
    pub async fn update_materialized_view_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        let mut def = self
            .get_materialized_view(txn, db_id, name)
            .await?
            .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", name))?;
        def.query = query.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize matview definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        let bindings_key = self.key(&encode_matview_bindings_key_v2(db_id, name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
            txn_delete(txn, bindings_key).await?;
            info!("Dropped materialized view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_materialized_views(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<MatViewDef>> {
        let prefix = encode_matview_prefix_v2(db_id);
        let bindings_prefix = encode_matview_bindings_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;
        let mut matviews = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !is_definition_key(key, &prefix, &bindings_prefix) {
                continue;
            }
            let def: MatViewDef = bincode::deserialize(pair.value())
                .context("Failed to deserialize matview definition")?;
            matviews.push(def);
        }
        Ok(matviews)
    }

    pub async fn get_materialized_view_relation_bindings(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<Vec<String>>> {
        let key = self.key(&encode_matview_bindings_key_v2(db_id, name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => {
                let bindings: Vec<String> = bincode::deserialize(&data)
                    .context("Failed to deserialize matview relation bindings")?;
                Ok(Some(bindings))
            }
            None => Ok(None),
        }
    }

    pub async fn set_materialized_view_relation_bindings(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        relation_bindings: Vec<String>,
    ) -> Result<()> {
        let key = self.key(&encode_matview_bindings_key_v2(db_id, name));
        let bindings = bincode::serialize(&relation_bindings)
            .context("Failed to serialize matview relation bindings")?;
        txn_put(txn, key, bindings).await?;
        Ok(())
    }
}

fn is_definition_key(key: &[u8], definition_prefix: &[u8], bindings_prefix: &[u8]) -> bool {
    key.starts_with(definition_prefix) && !key.starts_with(bindings_prefix)
}

#[cfg(test)]
mod tests {
    use super::is_definition_key;
    use crate::storage::encoding::{
        encode_matview_bindings_key_v2, encode_matview_bindings_prefix_v2, encode_matview_key_v2,
        encode_matview_prefix_v2, encode_view_bindings_key_v2, encode_view_bindings_prefix_v2,
        encode_view_key_v2, encode_view_prefix_v2,
    };

    #[test]
    fn view_key_classification_excludes_bindings_keys() {
        let db_id = 42;
        let def_prefix = encode_view_prefix_v2(db_id);
        let bindings_prefix = encode_view_bindings_prefix_v2(db_id);
        let def_key = encode_view_key_v2(db_id, "public.v");
        let bindings_key = encode_view_bindings_key_v2(db_id, "public.v");

        assert!(is_definition_key(&def_key, &def_prefix, &bindings_prefix));
        assert!(!is_definition_key(
            &bindings_key,
            &def_prefix,
            &bindings_prefix
        ));
    }

    #[test]
    fn matview_key_classification_excludes_bindings_keys() {
        let db_id = 42;
        let def_prefix = encode_matview_prefix_v2(db_id);
        let bindings_prefix = encode_matview_bindings_prefix_v2(db_id);
        let def_key = encode_matview_key_v2(db_id, "public.mv");
        let bindings_key = encode_matview_bindings_key_v2(db_id, "public.mv");

        assert!(is_definition_key(&def_key, &def_prefix, &bindings_prefix));
        assert!(!is_definition_key(
            &bindings_key,
            &def_prefix,
            &bindings_prefix
        ));
    }
}
