use super::*;

impl EmbeddedPageFs {
    pub(crate) async fn write_file(
        &self,
        path: &str,
        data: &[u8],
        mode: Option<u32>,
    ) -> Result<usize> {
        if !data.is_empty() && !can_store_inline_len(data.len()) {
            if !self.has_object_storage() {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "files larger than {} bytes require S3-backed object storage",
                    fs9_config().inline_max_bytes
                ))));
            }

            let mut writer = self
                .begin_write_stream(
                    path,
                    FsWriteStreamOptions {
                        expected_size: Some(u64::try_from(data.len()).map_err(|_| {
                            anyhow!(EmbeddedFsError::internal("write length exceeds u64"))
                        })?),
                        mode,
                    },
                )
                .await?;
            if let Err(err) = writer.write_chunk(data).await {
                let _ = writer.abort().await;
                return Err(err);
            }
            return writer.finish().await;
        }

        let mut txn = self.begin().await?;
        let prepared = prepare_replace_file_txn(self, &mut txn, path, mode).await?;
        let PreparedFile {
            inode_id,
            mut inode,
            parent_inode,
            is_new,
        } = prepared;

        if data.is_empty() {
            inode.data = DataRef::None;
            inode.size = 0;
        } else {
            blob::write_blob(&mut txn, inode_id, data).await?;
            inode.data = DataRef::InlineBlob;
            inode.size = data.len() as u64;
        }

        bump_inode_generation(&mut inode)?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;

        txn.commit().await?;

        // Emit fs9 notify event after successful commit.
        let normalized = normalize_path(path);
        self.emit_event(FsEventBuilder {
            event_type: if is_new {
                FsEventType::Create
            } else {
                FsEventType::Write
            },
            path: normalized,
            old_path: None,
            inode: inode_id,
            parent_inode,
            generation: inode.generation,
            is_dir: false,
            size: inode.size,
        });

        Ok(data.len())
    }

    pub(crate) async fn batch_write(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let s3_available = fs9_config().s3.is_some();
        let use_pack_route =
            s3_available && files.len() > 1 && files.iter().any(|file| !file.data.is_empty());
        if !use_pack_route {
            debug!(
                files = files.len(),
                s3_available, "fs9: batch_write using sequential route"
            );
            return self.batch_write_sequential(files).await;
        }
        debug!(files = files.len(), "fs9: batch_write using pack route");

        // Do NOT fall back to sequential on pack failure: internal retries
        // (5× TiKV write-conflict, 5× S3 HEAD verification) are already
        // exhausted inside batch_write_pack, and unconditional fallback after
        // a successful publish can corrupt data by overwriting pack-backed
        // inodes with inline blobs (see #2085 discussion).
        self.batch_write_pack(files).await
    }

    async fn batch_write_sequential(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path;
            let mode = file.mode;
            let result = self.write_file(&path, &file.data, mode).await;
            entries.push(FsBatchWriteEntry { path, result });
        }
        Ok(entries)
    }

    async fn batch_write_pack(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        let (bundle_id, key, staged_files) = self.prepare_pack_staging_batch(&files).await?;
        let staging_inode_ids: Vec<u64> = staged_files
            .iter()
            .map(|file| file.staging_inode_id)
            .collect();

        let build_inputs: Vec<BundleBuildInput> = staged_files
            .iter()
            .filter(|file| !file.data.is_empty())
            .map(|file| BundleBuildInput {
                staging_inode_id: file.staging_inode_id,
                data: file.data.clone(),
            })
            .collect();
        let built = match build_bundle(bundle_id, &build_inputs) {
            Ok(built) => built,
            Err(err) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(err);
            }
        };
        let journal = BundleJournal {
            keyspace: self.keyspace.clone(),
            fs_instance_id: self.runtime_state().fs_instance_id,
            bundle_id,
            key: key.clone(),
            created_at: current_unix_timestamp(),
            object_size: built.object_size,
            staging_inode_ids: staging_inode_ids.clone(),
        };
        if let Err(err) = self
            .bundle_spool
            .write_pending_bundle(&journal, &built)
            .await
        {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        let s3 = match self.s3_client().await {
            Ok(Some(s3)) => s3,
            Ok(None) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(anyhow!(EmbeddedFsError::internal("S3 is not configured")));
            }
            Err(err) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(err);
            }
        };

        if let Err(err) = s3.put_object(&key, built.bytes.clone()).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        if let Err(err) = head_object_with_retry(&s3, &key).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        let entry_by_inode: HashMap<u64, PackEntryRef> = built
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.staging_inode_id,
                    PackEntryRef {
                        bundle_id,
                        offset: entry.offset,
                        len: entry.len,
                        checksum: entry.checksum,
                        generation: entry.staging_inode_id,
                    },
                )
            })
            .collect();
        let publish_files: Vec<PreparedPackPublishFile> = staged_files
            .iter()
            .map(|file| PreparedPackPublishFile {
                path: file.path.clone(),
                staging_inode_id: file.staging_inode_id,
                size: file.data.len() as u64,
                pack_entry: entry_by_inode.get(&file.staging_inode_id).copied(),
            })
            .collect();
        let manifest = BundleManifest {
            fs_instance_id: self.runtime_state().fs_instance_id,
            bundle_id,
            key,
            created_at: current_unix_timestamp(),
            object_size: built.object_size,
            footer_offset: built.footer_offset,
            entry_count: u32::try_from(built.entries.len()).map_err(|_| {
                anyhow!(EmbeddedFsError::internal("bundle entry count exceeds u32"))
            })?,
            live_entries: u32::try_from(built.entries.len()).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "bundle live entry count exceeds u32"
                ))
            })?,
            live_bytes: publish_files
                .iter()
                .filter_map(|file| file.pack_entry.map(|_| file.size))
                .sum(),
            stale_entries: 0,
            checksum: built.checksum,
            state: BundleManifestState::Active,
        };

        if let Err(err) = self.publish_pack_batch(&manifest, &publish_files).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        if let Err(err) = self.bundle_spool.remove_bundle(bundle_id).await {
            warn!(
                "fs9: pack spool cleanup failed for bundle {bundle_id} after successful publish: {err}"
            );
        }
        Ok(publish_files
            .into_iter()
            .map(|file| FsBatchWriteEntry {
                path: file.path,
                result: Ok(file.size as usize),
            })
            .collect())
    }

    pub(crate) async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        // Validate early that the target is not a directory or symlink.
        let mut check_txn = self.begin().await?;
        match resolve_path(&mut check_txn, path).await {
            Ok((_, inode)) if inode.is_directory() => {
                let _ = check_txn.rollback().await;
                return Err(anyhow!(EmbeddedFsError::is_directory(path)));
            }
            Ok((_, inode)) if inode.is_symlink() => {
                let _ = check_txn.rollback().await;
                return Err(anyhow!(EmbeddedFsError::InvalidInput(
                    "cannot write to symlink as file; use readlink".to_string()
                )));
            }
            Ok(_) => {}
            Err(err) if !is_not_found_error(&err) => {
                let _ = check_txn.rollback().await;
                return Err(err);
            }
            Err(_) => {}
        }
        let _ = check_txn.rollback().await;

        let has_object_storage = self.has_object_storage();
        if opts
            .expected_size
            .is_some_and(|size| !can_store_inline_u64(size) && !has_object_storage)
        {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "files larger than {} bytes require S3-backed object storage",
                fs9_config().inline_max_bytes
            ))));
        }

        let want_object = should_use_direct_object_stream(opts.expected_size, has_object_storage);

        if want_object {
            let s3 = self
                .s3_client()
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
            let part_size = fs9_config()
                .s3
                .as_ref()
                .map(|c| c.multipart_part_bytes)
                .unwrap_or(WRITE_STREAM_FLUSH_BYTES);

            let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
            for attempt in 0..attempts {
                let mut txn = self.begin().await?;
                let inode_id = self.alloc_inode_id().await?;
                let key = self.object_key(inode_id)?;
                let upload_id = match s3.create_multipart_upload(&key).await {
                    Ok(id) => id,
                    Err(err) => {
                        let _ = txn.rollback().await;
                        return Err(err);
                    }
                };

                let now = current_unix_timestamp();
                let mut inode = Inode::new_file(inode_id, opts.mode.unwrap_or(0o644));
                inode.nlink = 0;
                inode.size = opts.expected_size.unwrap_or(0);
                inode.data = DataRef::Object {
                    key: key.clone(),
                    version: inode_id,
                    checksum: [0u8; 32],
                };
                save_inode(&mut txn, &inode).await?;
                lifecycle::save_lifecycle(
                    &mut txn,
                    inode_id,
                    &FileLifecycle::Uploading {
                        fs_instance_id: self.runtime_state().fs_instance_id,
                        upload_id: Some(upload_id.clone()),
                        updated_at: now,
                        reservation: None,
                    },
                )
                .await?;

                match txn.commit().await {
                    Ok(_) => {
                        return Ok(Box::new(EmbeddedObjectWriteStream {
                            fs: self.clone(),
                            s3,
                            path: normalize_path(path),
                            staging_inode_id: inode_id,
                            key,
                            upload_id,
                            part_size,
                            buffered: BytesMut::with_capacity(
                                part_size.min(WRITE_STREAM_FLUSH_BYTES),
                            ),
                            next_part_number: 1,
                            parts: Vec::new(),
                            bytes_written: 0,
                            hasher: Sha256::new(),
                            last_lifecycle_refresh: now,
                        }));
                    }
                    Err(err) if attempt + 1 < attempts => {
                        let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                        let err = anyhow!(err);
                        if is_retryable_tikv_write_conflict(&err) {
                            fs9_commit_backoff(attempt).await;
                            continue;
                        }
                        return Err(err);
                    }
                    Err(err) => {
                        let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                        return Err(anyhow!(err));
                    }
                }
            }

            return Err(anyhow!(EmbeddedFsError::internal(
                "begin_write_stream retry exhausted",
            )));
        }

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let inode_id = self.alloc_inode_id().await?;
            let mut inode = Inode::new_file(inode_id, opts.mode.unwrap_or(0o644));
            inode.nlink = 0;
            save_inode(&mut txn, &inode).await?;
            let now = current_unix_timestamp();
            mark_staging_write(&mut txn, inode_id, now).await?;

            match txn.commit().await {
                Ok(_) => {
                    return Ok(Box::new(EmbeddedStagingWriteStream {
                        fs: self.clone(),
                        path: normalize_path(path),
                        staging_inode_id: inode_id,
                        buffered: Vec::with_capacity(WRITE_STREAM_FLUSH_BYTES),
                        committed_bytes: 0,
                        last_staging_refresh: now,
                    }));
                }
                Err(err) if attempt + 1 < attempts => {
                    let err = anyhow!(err);
                    if is_retryable_tikv_write_conflict(&err) {
                        fs9_commit_backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(anyhow!(err)),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "begin_write_stream retry exhausted",
        )))
    }

    pub(crate) async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
    ) -> Result<FsCreateUpload> {
        if expected_size == 0 {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "create_upload requires a positive expected_size".to_string(),
            )));
        }

        let object_min = u64::try_from(fs9_config().object_min_bytes)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("FS9_OBJECT_MIN exceeds u64")))?;
        if expected_size < object_min {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "create_upload requires expected_size >= {} bytes",
                object_min
            ))));
        }

        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        let normalized = normalize_path(path);
        let part_size = fs9_config()
            .s3
            .as_ref()
            .map(|cfg| cfg.multipart_part_bytes)
            .unwrap_or(WRITE_STREAM_FLUSH_BYTES);

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let (expected_prior_inode, expected_prior_generation) =
                match resolve_path(&mut txn, &normalized).await {
                    Ok((inode_id, inode)) => {
                        if inode.is_directory() {
                            return Err(anyhow!(EmbeddedFsError::is_directory(&normalized)));
                        }
                        if inode.is_symlink() {
                            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                                "cannot write to symlink as file; use readlink".to_string()
                            )));
                        }
                        (Some(inode_id), Some(inode.generation))
                    }
                    Err(err) if is_not_found_error(&err) => (None, None),
                    Err(err) => return Err(err),
                };
            let expected_parent_inode = resolve_existing_parent(&mut txn, &normalized).await?;
            let inode_id = self.alloc_inode_id().await?;
            let object_key = self.object_key(inode_id)?;
            let upload_id = match s3.create_multipart_upload(&object_key).await {
                Ok(upload_id) => upload_id,
                Err(err) => {
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            };

            let now = current_unix_timestamp();
            let expires_at = now
                .checked_add(
                    i64::try_from(fs9_config().presign_ttl_secs).map_err(|_| {
                        anyhow!(EmbeddedFsError::internal("presign ttl exceeds i64"))
                    })?,
                )
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("presign expiry overflow")))?;
            let nonce = rand::thread_rng().gen_range(1..=u64::MAX);
            let path_hash = path_hash_bytes(&normalized);
            let reservation = UploadReservation {
                fs_instance_id: self.runtime_state().fs_instance_id,
                path: normalized.clone(),
                path_hash,
                expected_parent_inode,
                expected_prior_inode,
                expected_prior_generation,
                expected_size,
                nonce,
                expires_at,
            };
            let claims = UploadTokenClaims {
                keyspace: self.keyspace.clone(),
                fs_instance_id: self.runtime_state().fs_instance_id,
                staging_inode_id: inode_id,
                target_path_hash: normalized_path_hash_hex(&normalized),
                expected_parent_inode,
                expected_prior_inode,
                expected_prior_generation,
                upload_id: upload_id.clone(),
                target_version: inode_id,
                nonce,
                expires_at,
            };
            let upload_token = match sign_upload_token(&claims) {
                Ok(token) => token,
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            };

            let mut inode = Inode::new_file(inode_id, mode.unwrap_or(0o644));
            inode.nlink = 0;
            inode.size = expected_size;
            inode.data = DataRef::Object {
                key: object_key.clone(),
                version: inode_id,
                checksum: [0u8; 32],
            };
            save_inode(&mut txn, &inode).await?;
            lifecycle::save_lifecycle(
                &mut txn,
                inode_id,
                &FileLifecycle::Uploading {
                    fs_instance_id: self.runtime_state().fs_instance_id,
                    upload_id: Some(upload_id.clone()),
                    updated_at: now,
                    reservation: Some(reservation),
                },
            )
            .await?;

            match txn.commit().await {
                Ok(_) => {
                    return Ok(FsCreateUpload {
                        upload_token,
                        upload_id,
                        part_size,
                        expires_at,
                    });
                }
                Err(err) if attempt + 1 < attempts => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    let err = anyhow!(err);
                    if is_retryable_tikv_write_conflict(&err) {
                        fs9_commit_backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    return Err(anyhow!(err));
                }
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "create_upload retry exhausted"
        )))
    }

    pub(crate) async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
    ) -> Result<FsPresignedRequest> {
        if !(1..=10_000).contains(&part_number) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "invalid multipart part number: {}",
                part_number
            ))));
        }

        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, true).await?;
        if ctx.phase != UploadLifecyclePhase::Uploading {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload is no longer in uploading state",
            )));
        }

        let max_part_number = max_presign_part_number(ctx.reservation.expected_size)?;
        if part_number > max_part_number {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "multipart part number {} exceeds per-upload limit {}",
                part_number, max_part_number
            ))));
        }

        let upload_id = ctx.upload_id.as_deref().ok_or_else(|| {
            anyhow!(EmbeddedFsError::internal(
                "uploading state missing upload_id"
            ))
        })?;
        let ttl_secs = presign_ttl_secs_from_claims(claims.expires_at)?;
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        s3.presign_upload_part(&ctx.key, upload_id, part_number, ttl_secs)
            .await
    }

    pub(crate) async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, false).await?;
        if ctx.phase == UploadLifecyclePhase::Published {
            return usize::try_from(ctx.inode.size)
                .map_err(|_| anyhow!(EmbeddedFsError::internal("published size exceeds usize")));
        }

        let completed_parts = normalize_completed_parts(parts)?;
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        if ctx.phase == UploadLifecyclePhase::Uploading {
            let max_part_number = max_presign_part_number(ctx.reservation.expected_size)?;
            if completed_parts
                .last()
                .is_some_and(|part| part.part_number > max_part_number)
            {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "multipart part number exceeds per-upload limit {}",
                    max_part_number
                ))));
            }

            let upload_id = ctx.upload_id.as_deref().ok_or_else(|| {
                anyhow!(EmbeddedFsError::internal(
                    "uploading state missing upload_id"
                ))
            })?;
            s3.complete_multipart_upload(
                &ctx.key,
                upload_id,
                completed_parts
                    .iter()
                    .map(|part| (part.part_number, part.etag.clone()))
                    .collect(),
            )
            .await?;
        }

        let head = head_object_with_retry(&s3, &ctx.key).await?;
        if head.size != ctx.reservation.expected_size {
            return Err(anyhow!(EmbeddedFsError::conflict(&format!(
                "uploaded object size {} does not match reserved size {}",
                head.size, ctx.reservation.expected_size
            ))));
        }

        self.mark_object_staging_committing(
            ctx.staging_inode_id,
            &ctx.key,
            head.size,
            checksum.unwrap_or([0u8; 32]),
            Some(ctx.reservation.clone()),
        )
        .await?;

        self.publish_staged_write_with_reservation(
            &ctx.reservation.path,
            ctx.staging_inode_id,
            Some(&ctx.reservation),
        )
        .await
    }

    pub(crate) async fn abort_upload(&self, upload_token: &str) -> Result<()> {
        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, false).await?;
        if ctx.phase != UploadLifecyclePhase::Uploading {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload can only be aborted while uploading",
            )));
        }

        self.abort_staged_write(ctx.staging_inode_id).await
    }
}
