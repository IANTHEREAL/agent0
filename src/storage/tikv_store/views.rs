use super::*;
use crate::sql::error::SqlError;

impl TikvStore {
    pub async fn create_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
        deps: Vec<String>,
        or_replace: bool,
    ) -> Result<()> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
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
            info!("Replaced view '{}'", name);
            return Ok(());
        }

        let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
        let oid = self.next_view_oid(txn, db_id).await?;
        let def = ViewDef {
            oid,
            schema: schema.to_string(),
            name: view_name.to_string(),
            query: query.to_string(),
            deps,
        };
        let data = bincode::serialize(&def).context("Failed to serialize view definition")?;
        txn_put(txn, key, data).await?;
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
        match txn.get(key).await? {
            Some(data) => {
                let def: ViewDef =
                    bincode::deserialize(&data).context("Failed to deserialize view definition")?;
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub async fn drop_view(&self, txn: &mut Transaction, db_id: u64, name: &str) -> Result<bool> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_views(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<ViewDef>> {
        let prefix = encode_view_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut views = Vec::new();
        for pair in pairs {
            let key_bytes: &[u8] = pair.key().as_ref().into();
            if !key_bytes.starts_with(&prefix) {
                continue;
            }
            let def: ViewDef = bincode::deserialize(pair.value())
                .context("Failed to deserialize view definition")?;
            views.push(def);
        }
        Ok(views)
    }

    pub async fn create_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
        deps: Vec<String>,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
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
        match txn.get(key).await? {
            Some(data) => {
                let def: MatViewDef = bincode::deserialize(&data)
                    .context("Failed to deserialize matview definition")?;
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub async fn drop_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
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
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut matviews = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let def: MatViewDef = bincode::deserialize(pair.value())
                .context("Failed to deserialize matview definition")?;
            matviews.push(def);
        }
        Ok(matviews)
    }
}
