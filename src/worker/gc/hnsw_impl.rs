use super::*;

impl WorkerGc {
    /// Sweep orphaned HNSW S3 graph objects.
    ///
    /// Sweep orphaned or retired HNSW S3 graph objects.
    ///
    /// Correctness rule:
    /// - Anything that was ever referenced by committed TiKV metadata must be
    ///   reclaimed using a safepoint-aware marker, never wall-clock age.
    /// - Objects without committed TiKV lifecycle state are treated as
    ///   speculative uploads and left leak-safe for now. Writers upload to S3
    ///   before committing TiKV metadata, so GC must not guess whether a
    ///   no-meta object will later become live.
    pub(super) async fn sweep_hnsw_s3_orphans(&self) -> Result<()> {
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
        let mut txn = self.system_store.begin().await?;
        let all_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_deleted = 0u64;

        for entry in &all_entries {
            // List all S3 objects for this (keyspace, db_id).
            let all_objects = match s3.list_objects(&entry.keyspace, entry.db_id).await {
                Ok(objs) => objs,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to list objects"
                    );
                    continue;
                }
            };

            if all_objects.is_empty() {
                continue;
            }

            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to acquire tenant store"
                    );
                    continue;
                }
            };
            let store = handle.store();

            // Group objects by (table_id, index_id).
            let mut index_objects: HashMap<(u64, u64), Vec<&crate::sql::hnsw::s3::S3ObjectInfo>> =
                HashMap::new();
            for obj in &all_objects {
                if let Some((table_id, index_id, _version)) =
                    crate::sql::hnsw::s3::parse_s3_key(&obj.key)
                {
                    index_objects
                        .entry((table_id, index_id))
                        .or_default()
                        .push(obj);
                }
            }

            // Read all HNSW metas for this database.
            let metas = match self.read_all_hnsw_metas(store.as_ref(), entry.db_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to read HNSW metas"
                    );
                    continue;
                }
            };

            // Process each index's objects.
            for ((table_id, index_id), objects) in &index_objects {
                let meta_ref = metas.get(&(*table_id, *index_id));
                let prefix_gc = match self
                    .read_hnsw_s3_prefix_gc_marker(
                        store.as_ref(),
                        entry.db_id,
                        *table_id,
                        *index_id,
                    )
                    .await
                {
                    Ok(marker) => marker,
                    Err(e) => {
                        warn!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            error = %e,
                            "HNSW S3 sweep: failed to read prefix GC marker"
                        );
                        continue;
                    }
                };

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
                                debug!(
                                    keyspace = %entry.keyspace,
                                    db_id = entry.db_id,
                                    table_id,
                                    index_id,
                                    gc_safepoint,
                                    delete_after = marker.delete_after_safepoint,
                                    "HNSW S3 sweep: cleaned whole prefix after safepoint advanced"
                                );
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
                        // No committed meta. This could be:
                        // (a) a writer-owned upload not yet committed (in-flight
                        //     CREATE INDEX inside an explicit transaction)
                        // (b) a rollback orphan (BEGIN; CREATE INDEX; ROLLBACK;)
                        // (c) a crash orphan (S3 PUT succeeded, TiKV commit crashed)
                        //
                        // We CANNOT safely use wall-clock age to distinguish these:
                        // an explicit transaction can hold an uncommitted S3 upload
                        // for longer than gc_life_time_sec. Deleting based on age
                        // would destroy a graph that the user is about to COMMIT.
                        //
                        // Correctness > cleanup: leave these objects alone. The S3
                        // storage cost of orphans is negligible. Rollback/crash
                        // orphans are bounded (one per failed CREATE INDEX) and
                        // will be overwritten if the same index is re-created.
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
                                    // Writers upload S3 graphs before committing the TiKV
                                    // metadata flip to the new graph_version. Therefore a
                                    // version greater than current_version may still be the
                                    // next live graph in-flight; GC must never infer
                                    // retirement from object listing alone for this case.
                                    //
                                    // Future/speculative versions are left alone. They
                                    // could be an in-flight merge upload or an uncommitted
                                    // explicit transaction. A crashed merge will overwrite
                                    // on retry; a rolled-back txn leaves a small orphan.
                                    // Correctness > cleanup.
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
                                            // Delete S3 object, then marker. On S3
                                            // failure, keep the marker (retry next
                                            // sweep) but do NOT abort the entire
                                            // sweep — other indexes must still be
                                            // processed.
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
        }

        if total_deleted > 0 {
            info!(total_deleted, "HNSW S3 sweep complete");
        }

        Ok(())
    }

    /// Read all HNSW metas for a database by enumerating schemas.
    ///
    /// Discovers HNSW indexes from table schemas (O(tables × indexes)),
    /// then point-gets each meta key (~200 bytes). Does NOT scan the
    /// d_{db}_hnsw_* prefix — that prefix contains millions of rowid
    /// mapping and delta keys on large tables.
    async fn read_all_hnsw_metas(
        &self,
        store: &TikvStore,
        db_id: u64,
    ) -> Result<HashMap<(u64, u64), crate::sql::hnsw::storage::HnswMeta>> {
        let mut txn = store.begin().await?;
        let result = async {
            let table_names = store.list_tables(&mut txn, db_id).await?;
            let schemas = store
                .list_table_schemas(&mut txn, db_id, &table_names)
                .await?;

            let mut result = HashMap::new();
            for schema in &schemas {
                for index in &schema.indexes {
                    if !index.is_hnsw() {
                        continue;
                    }
                    let meta_key =
                        crate::sql::hnsw::storage::hnsw_meta_key(db_id, schema.table_id, index.id);
                    if let Some(value) = txn.get(meta_key).await? {
                        if let Ok(meta) =
                            serde_json::from_slice::<crate::sql::hnsw::storage::HnswMeta>(&value)
                        {
                            result.insert((schema.table_id, index.id), meta);
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(result)
        }
        .await;

        match result {
            Ok(result) => {
                txn.rollback().await.ok();
                Ok(result)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
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
        let mut txn = store.begin().await?;
        let result = async {
            let range: tikv_client::BoundRange =
                (crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix(
                    db_id, table_id, index_id,
                )
                    ..crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix_end(
                        db_id, table_id, index_id,
                    ))
                    .into();
            let pairs = txn.scan(range, u32::MAX).await?;
            let keys: Vec<Vec<u8>> = pairs
                .map(|pair| {
                    let key: &[u8] = pair.key().as_ref().into();
                    key.to_vec()
                })
                .collect();
            for key in keys {
                crate::txn::txn_delete(&mut txn, key).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                Ok(())
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
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
        txn.commit().await?;
        Ok(())
    }
}
