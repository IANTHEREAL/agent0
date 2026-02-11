use super::*;

impl TikvStore {
    pub async fn create_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
        or_replace: bool,
    ) -> Result<()> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            if !or_replace {
                return Err(anyhow!("View '{}' already exists", name));
            }

            let mut def = self
                .get_view(txn, db_id, name)
                .await?
                .ok_or_else(|| anyhow!("View '{}' does not exist", name))?;
            def.query = query.to_string();
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
        match txn.get(key.clone()).await? {
            Some(data) => match bincode::deserialize::<ViewDef>(&data) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn, db_id).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    Ok(Some(def))
                }
                Err(_) => {
                    let query = String::from_utf8(data)?;
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
                    let oid = self.next_view_oid(txn, db_id).await?;
                    let def = ViewDef {
                        oid,
                        schema: schema.to_string(),
                        name: view_name.to_string(),
                        query,
                    };
                    let updated = bincode::serialize(&def).context("Failed to serialize view")?;
                    txn_put(txn, key, updated).await?;
                    Ok(Some(def))
                }
            },
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
            let name = String::from_utf8_lossy(&key_bytes[prefix.len()..]).to_string();
            let key = self.key(&encode_view_key_v2(db_id, &name));

            match bincode::deserialize::<ViewDef>(pair.value()) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn, db_id).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    views.push(def);
                }
                Err(_) => {
                    let query = String::from_utf8_lossy(pair.value()).to_string();
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", &name));
                    let oid = self.next_view_oid(txn, db_id).await?;
                    let def = ViewDef {
                        oid,
                        schema: schema.to_string(),
                        name: view_name.to_string(),
                        query,
                    };
                    let updated = bincode::serialize(&def).context("Failed to serialize view")?;
                    txn_put(txn, key, updated).await?;
                    views.push(def);
                }
            }
        }
        Ok(views)
    }

    pub async fn create_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Materialized view '{}' already exists", name));
        }
        txn_put(txn, key, query.as_bytes().to_vec()).await?;
        info!("Created materialized view '{}'", name);
        Ok(())
    }

    pub async fn get_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
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
    ) -> Result<Vec<String>> {
        let prefix = encode_matview_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut matviews = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                matviews.push(name);
            }
        }
        Ok(matviews)
    }
}
