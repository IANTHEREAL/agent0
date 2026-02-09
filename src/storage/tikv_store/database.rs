use super::*;

impl TikvStore {
    /// Allocate and persist the next database ID (storage format v2).
    pub async fn next_database_id(&self, txn: &mut Transaction) -> Result<u64> {
        const FIRST_DATABASE_ID: u64 = 1;

        let key = self.key(&encode_next_database_id_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                let id = u64::from_be_bytes(bytes);
                id.checked_add(1)
                    .ok_or_else(|| anyhow!("Database ID overflow"))?
            }
            None => FIRST_DATABASE_ID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    /// Look up a database ID by name (storage format v2).
    pub async fn get_database_id(
        &self,
        txn: &mut Transaction,
        db_name: &str,
    ) -> Result<Option<u64>> {
        let key = self.key(&encode_database_name_key(db_name));
        match txn.get(key).await? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                Ok(Some(u64::from_be_bytes(bytes)))
            }
            None => Ok(None),
        }
    }

    /// Fetch a database definition by ID (storage format v2).
    pub async fn get_database_by_id(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseDef>> {
        let key = self.key(&encode_database_id_key(db_id));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize database definition")?,
            )),
            None => Ok(None),
        }
    }

    /// List all databases in the current keyspace (storage format v2).
    pub async fn list_databases(&self, txn: &mut Transaction) -> Result<Vec<DatabaseDef>> {
        let prefix = encode_database_id_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut dbs = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let def: DatabaseDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize database")?;
            dbs.push(def);
        }
        Ok(dbs)
    }

    /// Create a new database (storage format v2).
    ///
    /// Returns `Ok(None)` if the database exists and `if_not_exists` is true.
    pub async fn create_database(
        &self,
        txn: &mut Transaction,
        name: &str,
        owner: &str,
        if_not_exists: bool,
    ) -> Result<Option<DatabaseDef>> {
        let name_key = self.key(&encode_database_name_key(name));
        if txn.get(name_key.clone()).await?.is_some() {
            if if_not_exists {
                return Ok(None);
            }
            return Err(anyhow!("database \"{}\" already exists", name));
        }

        let db_id = self.next_database_id(txn).await?;
        let def = DatabaseDef::new(db_id, name.to_string(), owner.to_string());

        txn_put(txn, name_key, db_id.to_be_bytes().to_vec()).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;

        info!("Created database '{}' with ID {}", name, db_id);
        Ok(Some(def))
    }

    /// Drop database metadata (storage format v2).
    ///
    /// Returns `Ok(None)` if the database does not exist and `if_exists` is true.
    ///
    /// Note: this does NOT delete the database's data range. Call
    /// `unsafe_destroy_database_data(db_id)` after committing the transaction.
    pub async fn drop_database_metadata(
        &self,
        txn: &mut Transaction,
        db_name: &str,
        if_exists: bool,
        current_database_id: u64,
    ) -> Result<Option<u64>> {
        if db_name.eq_ignore_ascii_case("postgres")
            || db_name.eq_ignore_ascii_case("template0")
            || db_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!(
                "cannot drop database \"{}\": it is a system database",
                db_name
            ));
        }

        let name_key = self.key(&encode_database_name_key(db_name));
        let db_id = match txn.get(name_key.clone()).await? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => {
                if if_exists {
                    return Ok(None);
                }
                return Err(anyhow!("database \"{}\" does not exist", db_name));
            }
        };

        if db_id == current_database_id {
            return Err(anyhow!("cannot drop the currently open database"));
        }

        txn_delete(txn, name_key).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        txn_delete(txn, id_key).await?;

        Ok(Some(db_id))
    }

    /// Rename a database (storage format v2).
    pub async fn rename_database(
        &self,
        txn: &mut Transaction,
        old_name: &str,
        new_name: &str,
        current_database_id: u64,
    ) -> Result<()> {
        if old_name.eq_ignore_ascii_case("postgres")
            || old_name.eq_ignore_ascii_case("template0")
            || old_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!("cannot rename database \"{}\"", old_name));
        }
        if new_name.eq_ignore_ascii_case("postgres")
            || new_name.eq_ignore_ascii_case("template0")
            || new_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!("cannot rename database to \"{}\"", new_name));
        }

        let old_key = self.key(&encode_database_name_key(old_name));
        let db_id = match txn.get(old_key.clone()).await? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => return Err(anyhow!("database \"{}\" does not exist", old_name)),
        };

        if db_id == current_database_id {
            return Err(anyhow!("cannot rename the currently open database"));
        }

        let new_key = self.key(&encode_database_name_key(new_name));
        if txn.get(new_key.clone()).await?.is_some() {
            return Err(anyhow!("database \"{}\" already exists", new_name));
        }

        txn_delete(txn, old_key).await?;
        txn_put(txn, new_key, db_id.to_be_bytes().to_vec()).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        let mut def = self
            .get_database_by_id(txn, db_id)
            .await?
            .ok_or_else(|| anyhow!("database metadata corrupted"))?;
        def.name = new_name.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;

        Ok(())
    }

    /// Update the owner for a database (storage format v2).
    pub async fn set_database_owner(
        &self,
        txn: &mut Transaction,
        db_name: &str,
        new_owner: &str,
    ) -> Result<()> {
        let name_key = self.key(&encode_database_name_key(db_name));
        let db_id = match txn.get(name_key).await? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => return Err(anyhow!("database \"{}\" does not exist", db_name)),
        };

        let id_key = self.key(&encode_database_id_key(db_id));
        let mut def = self
            .get_database_by_id(txn, db_id)
            .await?
            .ok_or_else(|| anyhow!("database metadata corrupted"))?;
        def.owner = new_owner.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;
        Ok(())
    }

    /// Efficiently delete all data within a database using TiKV's `unsafe_destroy_range`.
    ///
    /// This is a non-transactional, best-effort cleanup step intended for DROP DATABASE/TABLE.
    pub async fn unsafe_destroy_database_data(&self, db_id: u64) -> Result<()> {
        let (start, end) = encode_database_data_range(db_id);
        let range: BoundRange = (start..end).into();
        self.client()
            .unsafe_destroy_range(range)
            .await
            .map_err(|e| anyhow!(e))
    }
}
