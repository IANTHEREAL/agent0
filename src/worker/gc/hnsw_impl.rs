use super::*;
use std::collections::HashSet;

const HNSW_S3_MARKER_DELETE_BATCH_SIZE: u32 = 256;
const HNSW_S3_INTENT_GC_PAGE_SIZE: usize = 256;

pub(super) fn group_hnsw_s3_objects_by_index<I>(
    objects: I,
    prefix_deleted: &HashSet<(u64, u64)>,
) -> HashMap<(u64, u64), Vec<crate::sql::hnsw::s3::S3ObjectInfo>>
where
    I: IntoIterator<Item = crate::sql::hnsw::s3::S3ObjectInfo>,
{
    let mut index_objects: HashMap<(u64, u64), Vec<crate::sql::hnsw::s3::S3ObjectInfo>> =
        HashMap::new();
    for obj in objects {
        if let Some((table_id, index_id, _version)) = crate::sql::hnsw::s3::parse_s3_key(&obj.key) {
            if prefix_deleted.contains(&(table_id, index_id)) {
                continue;
            }
            index_objects
                .entry((table_id, index_id))
                .or_default()
                .push(obj);
        }
    }
    index_objects
}

impl WorkerGc {
    pub(super) async fn cleanup_hnsw_s3_external_object_intents(&self) -> Result<()> {
        if crate::sql::hnsw::s3::hnsw_s3_client().is_none() {
            return Ok(());
        }
        let client = self
            .system_store
            .transaction_client()
            .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
        let gc_safepoint = client.get_gc_safepoint().await?;

        self.cleanup_hnsw_s3_graph_upload_intents(gc_safepoint)
            .await?;
        self.cleanup_hnsw_s3_db_prefix_cleanup_intents(gc_safepoint)
            .await?;
        Ok(())
    }

    async fn cleanup_hnsw_s3_graph_upload_intents(&self, gc_safepoint: u64) -> Result<()> {
        let s3 = crate::sql::hnsw::s3::hnsw_s3_client()
            .ok_or_else(|| anyhow::anyhow!("HNSW S3 client not available"))?;
        let mut cursor: Option<Vec<u8>> = None;

        loop {
            let (intents, next_cursor) = {
                let mut txn = self.system_store.begin().await?;
                let page = self
                    .system_store
                    .scan_hnsw_s3_graph_upload_intents_page(
                        &mut txn,
                        cursor.as_deref(),
                        HNSW_S3_INTENT_GC_PAGE_SIZE,
                    )
                    .await?;
                txn.rollback().await.ok();
                page
            };

            if intents.is_empty() {
                break;
            }

            for intent in intents {
                let meta = match self.pool.acquire(Some(intent.keyspace.clone())).await {
                    Ok(handle) => {
                        let store = handle.store().clone();
                        let mut txn = store.begin().await?;
                        let meta_key = crate::sql::hnsw::storage::hnsw_meta_key(
                            intent.db_id,
                            intent.table_id,
                            intent.index_id,
                        );
                        let meta = match txn.get(meta_key).await? {
                            Some(bytes) => serde_json::from_slice::<
                                crate::sql::hnsw::storage::HnswMeta,
                            >(&bytes)
                            .ok(),
                            None => None,
                        };
                        txn.rollback().await.ok();
                        meta
                    }
                    Err(e) => {
                        warn!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            error = %e,
                            "HNSW S3 upload intent GC: failed to acquire tenant store"
                        );
                        continue;
                    }
                };

                if let Some(meta) = meta.as_ref() {
                    if meta.graph_version == intent.version {
                        self.delete_hnsw_s3_graph_upload_intent(&intent).await?;
                        continue;
                    }
                    if meta.graph_version > intent.version {
                        // The upload committed and was later superseded. This
                        // object is historical, not speculative; retired-version
                        // GC owns its MVCC-safe deletion.
                        self.delete_hnsw_s3_graph_upload_intent(&intent).await?;
                        continue;
                    }
                }

                if gc_safepoint < intent.txn_start_ts {
                    continue;
                }

                match s3
                    .delete_graph(
                        &intent.keyspace,
                        intent.db_id,
                        intent.table_id,
                        intent.index_id,
                        intent.version,
                    )
                    .await
                {
                    Ok(()) => {
                        self.delete_hnsw_s3_graph_upload_intent(&intent).await?;
                        debug!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            table_id = intent.table_id,
                            index_id = intent.index_id,
                            version = intent.version,
                            "HNSW S3 upload intent GC: deleted uncommitted graph"
                        );
                    }
                    Err(e) => {
                        warn!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            table_id = intent.table_id,
                            index_id = intent.index_id,
                            version = intent.version,
                            error = %e,
                            "HNSW S3 upload intent GC: graph delete failed; intent retained"
                        );
                    }
                }
            }

            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(())
    }

    async fn cleanup_hnsw_s3_db_prefix_cleanup_intents(&self, gc_safepoint: u64) -> Result<()> {
        let s3 = crate::sql::hnsw::s3::hnsw_s3_client()
            .ok_or_else(|| anyhow::anyhow!("HNSW S3 client not available"))?;
        let mut cursor: Option<Vec<u8>> = None;

        loop {
            let (intents, next_cursor) = {
                let mut txn = self.system_store.begin().await?;
                let page = self
                    .system_store
                    .scan_hnsw_s3_db_prefix_cleanup_intents_page(
                        &mut txn,
                        cursor.as_deref(),
                        HNSW_S3_INTENT_GC_PAGE_SIZE,
                    )
                    .await?;
                txn.rollback().await.ok();
                page
            };

            if intents.is_empty() {
                break;
            }

            for intent in intents {
                let db_exists = match self.pool.acquire(Some(intent.keyspace.clone())).await {
                    Ok(handle) => {
                        let store = handle.store().clone();
                        let mut txn = store.begin().await?;
                        let exists = store
                            .get_database_by_id(&mut txn, intent.db_id)
                            .await?
                            .is_some();
                        txn.rollback().await.ok();
                        exists
                    }
                    Err(e) => {
                        warn!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            error = %e,
                            "HNSW S3 DB cleanup intent GC: failed to acquire tenant store"
                        );
                        continue;
                    }
                };

                if db_exists {
                    // A live DB is not proof that this intent is stale: DROP
                    // DATABASE records the durable cleanup intent before the
                    // tenant metadata deletion commits. If GC runs in that
                    // window, deleting the intent would break the crash-safe
                    // handoff and leak S3 objects if inline cleanup later
                    // fails. Keep the intent until either the DB is observably
                    // gone or the source DROP txn has crossed the GC safepoint.
                    if gc_safepoint < intent.drop_txn_start_ts {
                        debug!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            drop_txn_start_ts = intent.drop_txn_start_ts,
                            gc_safepoint,
                            "HNSW S3 DB cleanup intent GC: DB still exists and DROP txn may still commit; intent retained"
                        );
                        continue;
                    }
                    self.delete_hnsw_s3_db_prefix_cleanup_intent(&intent)
                        .await?;
                    debug!(
                        keyspace = %intent.keyspace,
                        db_id = intent.db_id,
                        drop_txn_start_ts = intent.drop_txn_start_ts,
                        gc_safepoint,
                        "HNSW S3 DB cleanup intent GC: removed stale live-DB intent after source txn crossed safepoint"
                    );
                    continue;
                }

                match s3.delete_db_prefix(&intent.keyspace, intent.db_id).await {
                    Ok(deleted) => {
                        self.delete_hnsw_s3_db_prefix_cleanup_intent(&intent)
                            .await?;
                        if deleted > 0 {
                            info!(
                                keyspace = %intent.keyspace,
                                db_id = intent.db_id,
                                deleted,
                                "HNSW S3 DB cleanup intent GC: deleted DB prefix"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            keyspace = %intent.keyspace,
                            db_id = intent.db_id,
                            error = %e,
                            "HNSW S3 DB cleanup intent GC: prefix delete failed; intent retained"
                        );
                    }
                }
            }

            cursor = next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(())
    }

    async fn delete_hnsw_s3_graph_upload_intent(
        &self,
        intent: &crate::worker::types::HnswS3GraphUploadIntent,
    ) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        self.system_store
            .delete_hnsw_s3_graph_upload_intent(
                &mut txn,
                &intent.keyspace,
                intent.db_id,
                intent.table_id,
                intent.index_id,
                intent.version,
            )
            .await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_db_prefix_cleanup_intent(
        &self,
        intent: &crate::worker::types::HnswS3DbPrefixCleanupIntent,
    ) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        self.system_store
            .delete_hnsw_s3_db_prefix_cleanup_intent(&mut txn, &intent.keyspace, intent.db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn sweep_hnsw_s3_orphans_for_entry(
        &self,
        entry: &crate::worker::types::TaskRegistryEntry,
        store: &Arc<TikvStore>,
    ) -> Result<()> {
        let s3 = crate::sql::hnsw::s3::hnsw_s3_client()
            .ok_or_else(|| anyhow::anyhow!("HNSW S3 client not available"))?;

        let client = self
            .system_store
            .transaction_client()
            .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
        let gc_safepoint = client.get_gc_safepoint().await?;
        let seal_safepoint = client
            .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
            .await
            .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?
            .version();

        let mut continuation_token: Option<String> = None;
        let mut metas: HashMap<(u64, u64), crate::sql::hnsw::storage::HnswMeta> = HashMap::new();
        let mut prefix_deleted: HashSet<(u64, u64)> = HashSet::new();
        let mut total_deleted = 0u64;

        loop {
            let page = s3
                .list_objects_page(&entry.keyspace, entry.db_id, continuation_token.take())
                .await?;
            let next_continuation_token = page.next_continuation_token;
            if page.objects.is_empty() {
                match next_continuation_token {
                    Some(token) => {
                        continuation_token = Some(token);
                        continue;
                    }
                    None => break,
                }
            }

            let index_objects = group_hnsw_s3_objects_by_index(page.objects, &prefix_deleted);
            if index_objects.is_empty() {
                match next_continuation_token {
                    Some(token) => {
                        continuation_token = Some(token);
                        continue;
                    }
                    None => break,
                }
            }

            let missing_metas = index_objects
                .keys()
                .filter(|idx| !metas.contains_key(idx))
                .copied()
                .collect::<Vec<_>>();
            if !missing_metas.is_empty() {
                metas.extend(
                    self.read_hnsw_metas_for_indexes(store.as_ref(), entry.db_id, &missing_metas)
                        .await?,
                );
            }

            for ((table_id, index_id), objects) in &index_objects {
                let meta_ref = metas.get(&(*table_id, *index_id));
                let prefix_gc = self
                    .read_hnsw_s3_prefix_gc_marker(
                        store.as_ref(),
                        entry.db_id,
                        *table_id,
                        *index_id,
                    )
                    .await?;

                if let Some(mut marker) = prefix_gc {
                    let can_delete_whole_prefix = meta_ref
                        .map(|meta| meta.dropped_at.is_some() || meta.graph_version == 0)
                        .unwrap_or(true);

                    if !can_delete_whole_prefix {
                        warn!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            current_version = meta_ref.map(|m| m.graph_version).unwrap_or(0),
                            "HNSW S3 sweep: deleting stale prefix GC marker on live index"
                        );
                        self.delete_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                        )
                        .await?;
                    } else if marker.delete_after_safepoint.is_none() {
                        marker.delete_after_safepoint = Some(seal_safepoint);
                        self.write_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                            &marker,
                        )
                        .await?;
                    } else if gc_safepoint >= marker.delete_after_safepoint.unwrap() {
                        match s3
                            .delete_prefix(&entry.keyspace, entry.db_id, *table_id, *index_id)
                            .await
                        {
                            Ok(count) => {
                                total_deleted += count;
                                prefix_deleted.insert((*table_id, *index_id));
                                self.delete_hnsw_s3_prefix_gc_marker(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                )
                                .await?;
                                self.delete_hnsw_s3_retired_version_markers_for_index(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                )
                                .await?;
                                if meta_ref.is_some_and(|m| m.dropped_at.is_some()) {
                                    self.delete_hnsw_meta(
                                        store.as_ref(),
                                        entry.db_id,
                                        *table_id,
                                        *index_id,
                                    )
                                    .await?;
                                }
                            }
                            Err(e) => {
                                warn!(
                                    keyspace = %entry.keyspace,
                                    db_id = entry.db_id,
                                    table_id,
                                    index_id,
                                    error = %e,
                                    "HNSW S3 sweep: failed to delete prefix"
                                );
                            }
                        }
                    }
                    if can_delete_whole_prefix {
                        continue;
                    }
                }

                match meta_ref {
                    None => {
                        debug!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            object_count = objects.len(),
                            "HNSW S3 sweep: leaving no-meta objects untouched \
                             (may be uncommitted explicit transaction)"
                        );
                    }
                    Some(meta) if meta.dropped_at.is_some() => {
                        let marker = crate::sql::hnsw::storage::HnswS3PrefixGc {
                            delete_after_safepoint: Some(seal_safepoint),
                            reason: Some("drop".to_string()),
                        };
                        self.write_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                            &marker,
                        )
                        .await?;
                    }
                    Some(meta) => {
                        let current_version = meta.graph_version;
                        if current_version == 0 {
                            let marker = crate::sql::hnsw::storage::HnswS3PrefixGc {
                                delete_after_safepoint: Some(seal_safepoint),
                                reason: Some("truncate".to_string()),
                            };
                            self.write_hnsw_s3_prefix_gc_marker(
                                store.as_ref(),
                                entry.db_id,
                                *table_id,
                                *index_id,
                                &marker,
                            )
                            .await?;
                            continue;
                        }

                        for obj in objects {
                            let version = match crate::sql::hnsw::s3::parse_s3_key(&obj.key) {
                                Some((_, _, v)) => v,
                                None => continue,
                            };
                            let retired_marker = self
                                .read_hnsw_s3_retired_version_marker(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                    version,
                                )
                                .await?;
                            match classify_live_hnsw_s3_version(
                                current_version,
                                version,
                                retired_marker.is_some(),
                            ) {
                                LiveHnswS3VersionDisposition::Current {
                                    clear_stale_retired_marker,
                                } => {
                                    if clear_stale_retired_marker {
                                        self.delete_hnsw_s3_retired_version_marker(
                                            store.as_ref(),
                                            entry.db_id,
                                            *table_id,
                                            *index_id,
                                            version,
                                        )
                                        .await?;
                                        warn!(
                                            keyspace = %entry.keyspace,
                                            db_id = entry.db_id,
                                            table_id,
                                            index_id,
                                            version,
                                            "HNSW S3 sweep: removed stale retired marker from current live version"
                                        );
                                    }
                                }
                                LiveHnswS3VersionDisposition::FutureSpeculative {
                                    clear_stale_retired_marker,
                                } => {
                                    if clear_stale_retired_marker {
                                        self.delete_hnsw_s3_retired_version_marker(
                                            store.as_ref(),
                                            entry.db_id,
                                            *table_id,
                                            *index_id,
                                            version,
                                        )
                                        .await?;
                                        warn!(
                                            keyspace = %entry.keyspace,
                                            db_id = entry.db_id,
                                            table_id,
                                            index_id,
                                            current_version,
                                            version,
                                            "HNSW S3 sweep: removed stale retired marker from speculative future version"
                                        );
                                    }
                                }
                                LiveHnswS3VersionDisposition::HistoricalRetired => {
                                    match retired_marker {
                                        None => {
                                            let marker =
                                                crate::sql::hnsw::storage::HnswS3RetiredVersionGc {
                                                    delete_after_safepoint: Some(seal_safepoint),
                                                };
                                            self.write_hnsw_s3_retired_version_marker(
                                                store.as_ref(),
                                                entry.db_id,
                                                *table_id,
                                                *index_id,
                                                version,
                                                &marker,
                                            )
                                            .await?;
                                        }
                                        Some(mut marker)
                                            if marker.delete_after_safepoint.is_none() =>
                                        {
                                            marker.delete_after_safepoint = Some(seal_safepoint);
                                            self.write_hnsw_s3_retired_version_marker(
                                                store.as_ref(),
                                                entry.db_id,
                                                *table_id,
                                                *index_id,
                                                version,
                                                &marker,
                                            )
                                            .await?;
                                        }
                                        Some(marker)
                                            if gc_safepoint
                                                >= marker
                                                    .delete_after_safepoint
                                                    .unwrap_or(u64::MAX) =>
                                        {
                                            match s3
                                                .delete_graph(
                                                    &entry.keyspace,
                                                    entry.db_id,
                                                    *table_id,
                                                    *index_id,
                                                    version,
                                                )
                                                .await
                                            {
                                                Ok(()) => {
                                                    self.delete_hnsw_s3_retired_version_marker(
                                                        store.as_ref(),
                                                        entry.db_id,
                                                        *table_id,
                                                        *index_id,
                                                        version,
                                                    )
                                                    .await?;
                                                    total_deleted += 1;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        table_id,
                                                        index_id,
                                                        version,
                                                        error = %e,
                                                        "HNSW S3 sweep: retired version delete failed, marker retained for retry"
                                                    );
                                                }
                                            }
                                        }
                                        Some(_) => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }

            match next_continuation_token {
                Some(token) => continuation_token = Some(token),
                None => break,
            }
        }

        if total_deleted > 0 {
            info!(
                keyspace = %entry.keyspace,
                db_id = entry.db_id,
                total_deleted,
                "HNSW S3 sweep entry complete"
            );
        }

        Ok(())
    }

    async fn read_hnsw_metas_for_indexes(
        &self,
        store: &TikvStore,
        db_id: u64,
        indexes: &[(u64, u64)],
    ) -> Result<HashMap<(u64, u64), crate::sql::hnsw::storage::HnswMeta>> {
        let mut txn = store.begin().await?;
        let result = async {
            let mut result = HashMap::new();
            for (table_id, index_id) in indexes {
                let meta_key =
                    crate::sql::hnsw::storage::hnsw_meta_key(db_id, *table_id, *index_id);
                if let Some(value) = txn.get(meta_key).await? {
                    if let Ok(meta) =
                        serde_json::from_slice::<crate::sql::hnsw::storage::HnswMeta>(&value)
                    {
                        result.insert((*table_id, *index_id), meta);
                    }
                }
            }
            Ok::<_, anyhow::Error>(result)
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    async fn read_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<Option<crate::sql::hnsw::storage::HnswS3PrefixGc>> {
        let mut txn = store.begin().await?;
        let result = async {
            let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
            let Some(bytes) = txn.get(key).await? else {
                return Ok::<_, anyhow::Error>(None);
            };
            Ok(Some(serde_json::from_slice::<
                crate::sql::hnsw::storage::HnswS3PrefixGc,
            >(&bytes)?))
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    async fn write_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        marker: &crate::sql::hnsw::storage::HnswS3PrefixGc,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
        crate::txn::txn_put(&mut txn, key, serde_json::to_vec(marker)?).await?;
        store
            .assert_database_alive_for_update(&mut txn, db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
        crate::txn::txn_delete(&mut txn, key).await?;
        store
            .assert_database_alive_for_update(&mut txn, db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    async fn read_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> Result<Option<crate::sql::hnsw::storage::HnswS3RetiredVersionGc>> {
        let mut txn = store.begin().await?;
        let result = async {
            let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
                db_id, table_id, index_id, version,
            );
            let Some(bytes) = txn.get(key).await? else {
                return Ok::<_, anyhow::Error>(None);
            };
            Ok(Some(serde_json::from_slice::<
                crate::sql::hnsw::storage::HnswS3RetiredVersionGc,
            >(&bytes)?))
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    async fn write_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
        marker: &crate::sql::hnsw::storage::HnswS3RetiredVersionGc,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
            db_id, table_id, index_id, version,
        );
        crate::txn::txn_put(&mut txn, key, serde_json::to_vec(marker)?).await?;
        store
            .assert_database_alive_for_update(&mut txn, db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
            db_id, table_id, index_id, version,
        );
        crate::txn::txn_delete(&mut txn, key).await?;
        store
            .assert_database_alive_for_update(&mut txn, db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_retired_version_markers_for_index(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let prefix =
            crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix(db_id, table_id, index_id);
        let end = crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix_end(
            db_id, table_id, index_id,
        );
        let mut cursor = prefix;

        loop {
            let mut scan_txn = store.begin().await?;
            let keys_result: Result<Vec<Vec<u8>>> = async {
                let range: tikv_client::BoundRange = (cursor.clone()..end.clone()).into();
                let pairs = scan_txn
                    .scan(range, HNSW_S3_MARKER_DELETE_BATCH_SIZE)
                    .await?;
                Ok(pairs
                    .map(|pair| {
                        let key: &[u8] = pair.key().as_ref().into();
                        key.to_vec()
                    })
                    .collect())
            }
            .await;
            scan_txn.rollback().await.ok();
            let keys = keys_result?;
            if keys.is_empty() {
                break;
            }

            let scanned = keys.len();
            let mut next_cursor = keys
                .last()
                .cloned()
                .expect("non-empty retired marker batch has a last key");
            next_cursor.push(0x00);

            let mut delete_txn = store.begin().await?;
            let delete_result = async {
                for key in keys {
                    crate::txn::txn_delete(&mut delete_txn, key).await?;
                }
                store
                    .assert_database_alive_for_update(&mut delete_txn, db_id)
                    .await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;

            match delete_result {
                Ok(()) => {
                    delete_txn.commit().await?;
                }
                Err(e) => {
                    delete_txn.rollback().await.ok();
                    return Err(e);
                }
            }
            if scanned < HNSW_S3_MARKER_DELETE_BATCH_SIZE as usize {
                break;
            }
            cursor = next_cursor;
        }

        Ok(())
    }

    async fn delete_hnsw_meta(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let meta_key = crate::sql::hnsw::storage::hnsw_meta_key(db_id, table_id, index_id);
        crate::txn::txn_delete(&mut txn, meta_key).await?;
        store
            .assert_database_alive_for_update(&mut txn, db_id)
            .await?;
        txn.commit().await?;
        Ok(())
    }
}
