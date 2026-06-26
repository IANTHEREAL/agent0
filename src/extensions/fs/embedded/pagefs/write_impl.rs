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
            let outcome = writer.write_chunk(data).await;
            return writer.terminate(outcome).await;
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
            entries.push(FsBatchWriteEntry {
                path,
                result,
                failure_category: None,
            });
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
                failure_category: None,
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
                let upload_id = match s3.create_multipart_upload(&key, None).await {
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
                            guard: crate::extensions::fs::termination_guard::TerminationGuard::new(
                                "EmbeddedObjectWriteStream",
                            ),
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
                        guard: crate::extensions::fs::termination_guard::TerminationGuard::new(
                            "EmbeddedStagingWriteStream",
                        ),
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
        checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload> {
        let checksum_algorithm = match checksum_algorithm {
            Some(alg) if alg.eq_ignore_ascii_case("crc32c") => Some("crc32c"),
            Some(alg) => {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "unsupported checksum algorithm: {}; only 'crc32c' is supported",
                    alg
                ))));
            }
            None => None,
        };

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
            let upload_id = match s3
                .create_multipart_upload(&object_key, checksum_algorithm)
                .await
            {
                Ok(upload_id) => upload_id,
                Err(err) => {
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            };

            let now = current_unix_timestamp();
            let expires_at =
                now.checked_add(i64::try_from(fs9_config().upload_token_ttl_secs).map_err(
                    |_| anyhow!(EmbeddedFsError::internal("upload token ttl exceeds i64")),
                )?)
                .ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal("upload token expiry overflow"))
                })?;
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
                        checksum_algorithm: checksum_algorithm.map(|s| s.to_string()),
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
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        if !(1..=10_000).contains(&part_number) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "invalid multipart part number: {}",
                part_number
            ))));
        }

        let claims = verify_upload_token(upload_token)?;

        // Retry lifecycle refresh WriteConflict: two concurrent presign
        // requests (or a presign racing with complete/abort) may both try
        // to write _fs_L{inode}.  Retrying gives a fresh snapshot so we
        // see the true current state.
        let ctx = retry_on_lifecycle_conflict(
            fs9_config().tikv_commit_retry_attempts.max(1),
            |_attempt| self.load_upload_context(&claims, true),
        )
        .await?;

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
        let ttl_secs = presign_part_ttl_secs();
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        s3.presign_upload_part(&ctx.key, upload_id, part_number, ttl_secs, checksum_crc32c)
            .await
    }

    pub(crate) async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        let claims = verify_upload_token(upload_token)?;
        // complete_upload is exempt from token expiry: the HMAC signature, nonce,
        // and upload_id binding provide sufficient authentication. Rejecting a
        // completion after hours of successful part uploads would waste all work.
        let ctx = self.load_upload_context_skip_expiry(&claims).await?;
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
            if let Err(complete_err) = s3
                .complete_multipart_upload(
                    &ctx.key,
                    upload_id,
                    completed_parts
                        .iter()
                        .map(|part| {
                            (
                                part.part_number,
                                part.etag.clone(),
                                part.checksum_crc32c.clone(),
                            )
                        })
                        .collect(),
                )
                .await
            {
                // CompleteMultipartUpload can time out after S3 has already
                // assembled the object. Fall through to HeadObject to check
                // whether the object actually exists before giving up.
                tracing::warn!(
                    key = %ctx.key,
                    upload_id = %upload_id,
                    error = %complete_err,
                    "fs9: CompleteMultipartUpload failed, checking HeadObject",
                );
                if head_object_with_retry(&s3, &ctx.key).await.is_err() {
                    return Err(complete_err);
                }
            }
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

    /// Write multiple small files grouped by parent directory, each group in a
    /// single TiKV transaction. This amortises the per-file txn overhead (begin,
    /// parent resolution, commit) across all files sharing the same directory.
    ///
    /// **Semantics**: per-subgroup atomic — files sharing a parent directory are
    /// chunked by `grouped_write_subgroup_size` and each chunk is committed in
    /// one TiKV transaction. All files within a subgroup succeed or fail together.
    /// Cross-subgroup (including cross-directory) partial success is possible.
    ///
    /// Files that are too large for inline storage are silently skipped and
    /// returned as errors so the caller can fall back to streaming for those.
    pub(crate) async fn batch_write_grouped(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<FsBatchWriteGroupedResult> {
        use std::collections::HashMap;

        if files.is_empty() {
            return Ok(FsBatchWriteGroupedResult {
                entries: Vec::new(),
                actual_subgroup_count: 0,
                total_retries: 0,
                retries_exhausted: 0,
            });
        }

        // --- Group files by parent directory path ---
        struct GroupedFile {
            original_index: usize,
            file_name: String,
            full_path: String,
            data: Vec<u8>,
            mode: Option<u32>,
        }

        let total_files = files.len();
        let mut groups: HashMap<String, Vec<GroupedFile>> = HashMap::new();
        for (idx, file) in files.into_iter().enumerate() {
            // Reject files too large for inline storage upfront
            if !file.data.is_empty() && !can_store_inline_len(file.data.len()) {
                // Will be filled in as error below
                groups
                    .entry("__oversized__".to_string())
                    .or_default()
                    .push(GroupedFile {
                        original_index: idx,
                        file_name: String::new(),
                        full_path: file.path,
                        data: file.data,
                        mode: file.mode,
                    });
                continue;
            }

            let normalized = normalize_path(&file.path);
            let (parent, name) = match normalized.rsplit_once('/') {
                Some((p, n)) => {
                    let parent = if p.is_empty() {
                        "/".to_string()
                    } else {
                        p.to_string()
                    };
                    (parent, n.to_string())
                }
                None => ("/".to_string(), normalized.clone()),
            };
            groups.entry(parent).or_default().push(GroupedFile {
                original_index: idx,
                file_name: name,
                full_path: file.path,
                data: file.data,
                mode: file.mode,
            });
        }

        // Pre-allocate results vector
        let mut results: Vec<Option<FsBatchWriteEntry>> = (0..total_files).map(|_| None).collect();

        // Handle oversized files as errors
        if let Some(oversized) = groups.remove("__oversized__") {
            for gf in oversized {
                results[gf.original_index] = Some(FsBatchWriteEntry {
                    path: gf.full_path,
                    result: Err(anyhow::anyhow!(
                        "file too large for grouped inline write ({}B > max); use streaming",
                        gf.data.len()
                    )),
                    failure_category: Some("planner.oversized"),
                });
            }
        }

        let subgroup_count = groups.len();
        debug!(
            total_files,
            subgroup_count, "fs9: batch_write_grouped starting"
        );

        // --- Process each directory group, splitting by subgroup_size ---
        let max_subgroup = fs9_config().grouped_write_subgroup_size;
        let retry_attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        let mut actual_subgroup_count = 0usize;
        let mut total_retries = 0usize;
        let mut retries_exhausted = 0usize;
        for (parent_dir, group_files) in &groups {
            // Split large directory groups into chunks of max_subgroup
            for chunk in group_files.chunks(max_subgroup) {
                actual_subgroup_count += 1;
                let chunk_size = chunk.len();
                let t0 = std::time::Instant::now();

                let txn_files: Vec<(String, &[u8], Option<u32>)> = chunk
                    .iter()
                    .map(|gf| (gf.file_name.clone(), gf.data.as_slice(), gf.mode))
                    .collect();

                // Retry loop: on txn_conflict, retry with exponential backoff
                // before returning failure to the client.
                let outcome = retry_subgroup_op(retry_attempts, |_attempt| {
                    self.write_directory_group_txn(parent_dir, &txn_files)
                })
                .await;

                if outcome.retried {
                    total_retries += outcome.attempts_used.saturating_sub(1) as usize;
                }

                match outcome.result {
                    Ok(written_sizes) => {
                        crate::metrics::record_batch_write_atomic_subgroup_latency(t0.elapsed());
                        let elapsed_ms = t0.elapsed().as_millis();
                        if outcome.retried {
                            debug!(
                                parent_dir,
                                chunk_size,
                                elapsed_ms,
                                attempts = outcome.attempts_used,
                                "fs9: directory subgroup committed after retry"
                            );
                        } else {
                            debug!(
                                parent_dir,
                                chunk_size, elapsed_ms, "fs9: directory subgroup committed"
                            );
                        }
                        for (gf, written) in chunk.iter().zip(written_sizes) {
                            results[gf.original_index] = Some(FsBatchWriteEntry {
                                path: gf.full_path.clone(),
                                result: Ok(written),
                                failure_category: None,
                            });
                        }
                    }
                    Err(err) => {
                        crate::metrics::record_batch_write_atomic_subgroup_latency(t0.elapsed());
                        let elapsed_ms = t0.elapsed().as_millis();
                        let category = classify_group_commit_error(&err);
                        if category == "execution.txn_conflict" {
                            retries_exhausted += 1;
                        }
                        warn!(
                            parent_dir,
                            chunk_size,
                            elapsed_ms,
                            category,
                            error = %err,
                            "fs9: directory subgroup commit failed"
                        );
                        let err_msg = err.to_string();
                        for gf in chunk {
                            results[gf.original_index] = Some(FsBatchWriteEntry {
                                path: gf.full_path.clone(),
                                result: Err(anyhow::anyhow!(
                                    "directory group commit failed: {}",
                                    err_msg
                                )),
                                failure_category: Some(category),
                            });
                        }
                    }
                }
            }
        }

        Ok(FsBatchWriteGroupedResult {
            entries: results.into_iter().map(|r| r.unwrap()).collect(),
            actual_subgroup_count,
            total_retries,
            retries_exhausted,
        })
    }

    /// Write all files in a single directory within one TiKV transaction.
    /// The parent directory is resolved once and all writes + metadata
    /// updates are committed atomically.
    ///
    /// `files` is a slice of (file_name, data, mode) tuples — all files must
    /// share the same parent directory (identified by `parent_path`).
    async fn write_directory_group_txn(
        &self,
        parent_path: &str,
        files: &[(String, &[u8], Option<u32>)],
    ) -> Result<Vec<usize>> {
        let mut txn = self.begin().await?;

        // Resolve the parent directory once for the entire group.
        let parent_inode = if parent_path == "/" {
            ROOT_INODE
        } else {
            let parts: Vec<&str> = parent_path.split('/').filter(|s| !s.is_empty()).collect();
            let mut current = ROOT_INODE;
            for part in parts {
                if let Some(next_id) = lookup(&mut txn, current, part).await? {
                    let next = load_inode(&mut txn, next_id)
                        .await?
                        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling dir entry")))?;
                    if !next.is_directory() {
                        return Err(anyhow!(EmbeddedFsError::not_directory(part)));
                    }
                    current = next_id;
                } else {
                    // Auto-create intermediate directories
                    let new_id = self.alloc_inode_id().await?;
                    let inode = Inode::new_directory(new_id, 0o755);
                    save_inode(&mut txn, &inode).await?;
                    link(&mut txn, current, part, new_id).await?;
                    current = new_id;
                }
            }
            current
        };

        // Write each file within the same transaction
        let mut written_sizes = Vec::with_capacity(files.len());
        let mut event_builders = Vec::with_capacity(files.len());

        for (file_name, data, mode) in files {
            // Check if file already exists under this parent
            let (inode_id, mut inode, is_new) =
                if let Some(existing_id) = lookup(&mut txn, parent_inode, file_name).await? {
                    let existing = load_inode(&mut txn, existing_id).await?.ok_or_else(|| {
                        anyhow!(EmbeddedFsError::not_found(&format!(
                            "{}/{}",
                            parent_path, file_name
                        )))
                    })?;
                    if existing.is_directory() {
                        return Err(anyhow!(EmbeddedFsError::is_directory(&format!(
                            "{}/{}",
                            parent_path, file_name
                        ))));
                    }
                    if existing.is_symlink() {
                        return Err(anyhow!(EmbeddedFsError::InvalidInput(
                            "cannot write to symlink as file".to_string()
                        )));
                    }
                    // Retire old data ref
                    retire_inode_data_ref(
                        &mut txn,
                        existing_id,
                        &existing.data,
                        self.runtime_state().fs_instance_id,
                    )
                    .await?;
                    let mut ino = existing;
                    ino.data = DataRef::None;
                    ino.size = 0;
                    (existing_id, ino, false)
                } else {
                    let new_id = self.alloc_inode_id().await?;
                    let ino = Inode::new_file(new_id, mode.unwrap_or(0o644));
                    link(&mut txn, parent_inode, file_name, new_id).await?;
                    (new_id, ino, true)
                };

            // Write blob data
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

            let full_path = if parent_path == "/" {
                format!("/{}", file_name)
            } else {
                format!("{}/{}", parent_path, file_name)
            };
            event_builders.push(FsEventBuilder {
                event_type: if is_new {
                    FsEventType::Create
                } else {
                    FsEventType::Write
                },
                path: full_path,
                old_path: None,
                inode: inode_id,
                parent_inode,
                generation: inode.generation,
                is_dir: false,
                size: inode.size,
            });

            written_sizes.push(data.len());
        }

        // Single atomic commit for the entire directory group
        txn.commit().await?;

        // Fire events after successful commit (emit_events handles both
        // in-memory ring and Redis persistence).
        self.emit_events(event_builders);

        Ok(written_sizes)
    }
}

/// Recursively check whether a TiKV error contains a write conflict.
/// Mirrors the logic in `is_retryable_tikv_write_conflict` (pagefs.rs)
/// but returns bool for use in classification.
// TODO: consolidate with is_retryable_tikv_write_conflict() in pagefs.rs
fn tikv_error_contains_write_conflict(err: &tikv_client::Error) -> bool {
    match err {
        tikv_client::Error::KeyError(key_error) => key_error.conflict.is_some(),
        tikv_client::Error::PessimisticLockError { inner, .. } => {
            tikv_error_contains_write_conflict(inner)
        }
        tikv_client::Error::UndeterminedError(_) => false,
        tikv_client::Error::ExtractedErrors(errors)
        | tikv_client::Error::MultipleKeyErrors(errors) => {
            errors.iter().any(tikv_error_contains_write_conflict)
        }
        _ => false,
    }
}

/// Retry an async operation that may hit a TiKV `WriteConflict` during the
/// lifecycle refresh in `load_upload_context`.  Extracted as a named helper
/// so both `presign_upload_part` and its regression tests share the exact
/// same retry logic.
///
/// This mirrors `retry_subgroup_op` but uses `is_retryable_tikv_write_conflict`
/// (the pagefs-level predicate) rather than the subgroup classifier.
async fn retry_on_lifecycle_conflict<F, Fut, T>(max_attempts: u32, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    for attempt in 0..max_attempts {
        match op(attempt).await {
            Ok(val) => return Ok(val),
            Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < max_attempts => {
                super::fs9_commit_backoff(attempt).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(anyhow!(EmbeddedFsError::internal(
        "presign_upload_part lifecycle refresh retry exhausted"
    )))
}

/// Classify a subgroup commit error into a stable `execution.*` category
/// by downcasting the error chain (no string parsing).
///
/// For TiKV container errors (`ExtractedErrors`, `MultipleKeyErrors`),
/// recursively inspects inner errors rather than blanket-labeling. An
/// `UndeterminedError` is terminal because retrying can double-apply writes.
fn classify_group_commit_error(err: &anyhow::Error) -> &'static str {
    // Check for TiKV write conflict via recursive inspection of the error chain.
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(tikv_error_contains_write_conflict)
    }) {
        return "execution.txn_conflict";
    }

    // Check for EmbeddedFsError variants
    for cause in err.chain() {
        if let Some(fs_err) = cause.downcast_ref::<EmbeddedFsError>() {
            return match fs_err {
                EmbeddedFsError::NotFound(_) | EmbeddedFsError::NotDirectory(_) => {
                    "execution.parent_not_found"
                }
                EmbeddedFsError::Conflict(_) | EmbeddedFsError::RestartRequired(_) => {
                    "execution.txn_conflict"
                }
                _ => "execution.other",
            };
        }
    }

    // Check for timeout-like errors (std::io::ErrorKind::TimedOut)
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
    }) {
        return "execution.timeout";
    }

    "execution.unknown"
}

/// Determine whether a subgroup commit error should be retried.
/// Only `execution.txn_conflict` is retryable.
fn is_retryable_subgroup_error(err: &anyhow::Error) -> bool {
    classify_group_commit_error(err) == "execution.txn_conflict"
}

/// Result of a subgroup retry loop.
#[derive(Debug)]
struct SubgroupRetryOutcome<T> {
    result: Result<T>,
    attempts_used: u32,
    retried: bool,
}

/// Execute `op` up to `max_attempts` times, retrying on retryable subgroup errors
/// with exponential backoff. Returns the outcome including retry accounting.
///
/// This is the core retry logic extracted for testability.
async fn retry_subgroup_op<F, Fut, T>(max_attempts: u32, mut op: F) -> SubgroupRetryOutcome<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..max_attempts {
        match op(attempt).await {
            Ok(val) => {
                return SubgroupRetryOutcome {
                    result: Ok(val),
                    attempts_used: attempt + 1,
                    retried: attempt > 0,
                };
            }
            Err(err) => {
                if is_retryable_subgroup_error(&err) && attempt + 1 < max_attempts {
                    super::fs9_commit_backoff(attempt).await;
                    last_err = Some(err);
                } else {
                    return SubgroupRetryOutcome {
                        result: Err(err),
                        attempts_used: attempt + 1,
                        retried: attempt > 0,
                    };
                }
            }
        }
    }
    // All attempts exhausted via retryable errors
    SubgroupRetryOutcome {
        result: Err(last_err.unwrap()),
        attempts_used: max_attempts,
        retried: max_attempts > 1,
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn classify_tikv_key_conflict() {
        let key_err =
            tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }));
        let err = anyhow::anyhow!(key_err);
        assert_eq!(classify_group_commit_error(&err), "execution.txn_conflict");
    }

    #[test]
    fn classify_tikv_container_with_conflict() {
        let key_err =
            tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }));
        let container = tikv_client::Error::ExtractedErrors(vec![key_err]);
        let err = anyhow::anyhow!(container);
        assert_eq!(classify_group_commit_error(&err), "execution.txn_conflict");
    }

    #[test]
    fn classify_tikv_container_without_conflict() {
        // A container with a non-conflict error should NOT be classified as txn_conflict
        let non_conflict = tikv_client::Error::InternalError {
            message: String::from("region not available"),
        };
        let container = tikv_client::Error::ExtractedErrors(vec![non_conflict]);
        let err = anyhow::anyhow!(container);
        assert_eq!(classify_group_commit_error(&err), "execution.unknown");
    }

    #[test]
    fn classify_undetermined_commit_outcome_is_not_txn_conflict() {
        let key_err =
            tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }));
        let err = anyhow::anyhow!(tikv_client::Error::UndeterminedError(Box::new(key_err)));
        assert_eq!(classify_group_commit_error(&err), "execution.unknown");
        assert!(!is_retryable_subgroup_error(&err));
    }

    #[test]
    fn classify_embedded_fs_not_found() {
        let err = anyhow::anyhow!(EmbeddedFsError::NotFound("/missing".to_string()));
        assert_eq!(
            classify_group_commit_error(&err),
            "execution.parent_not_found"
        );
    }

    #[test]
    fn classify_embedded_fs_conflict() {
        let err = anyhow::anyhow!(EmbeddedFsError::Conflict("restart".to_string()));
        assert_eq!(classify_group_commit_error(&err), "execution.txn_conflict");
    }

    #[test]
    fn classify_io_timeout() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out");
        let err = anyhow::anyhow!(io_err);
        assert_eq!(classify_group_commit_error(&err), "execution.timeout");
    }

    #[test]
    fn classify_unknown_error() {
        let err = anyhow::anyhow!("something unexpected");
        assert_eq!(classify_group_commit_error(&err), "execution.unknown");
    }

    // --- Retry decision tests ---

    #[test]
    fn retryable_on_txn_conflict() {
        let key_err =
            tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }));
        let err = anyhow::anyhow!(key_err);
        assert!(is_retryable_subgroup_error(&err));
    }

    #[test]
    fn retryable_on_embedded_fs_conflict() {
        let err = anyhow::anyhow!(EmbeddedFsError::Conflict("restart".to_string()));
        assert!(is_retryable_subgroup_error(&err));
    }

    #[test]
    fn not_retryable_on_timeout() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out");
        let err = anyhow::anyhow!(io_err);
        assert!(!is_retryable_subgroup_error(&err));
    }

    #[test]
    fn not_retryable_on_parent_not_found() {
        let err = anyhow::anyhow!(EmbeddedFsError::NotFound("/missing".to_string()));
        assert!(!is_retryable_subgroup_error(&err));
    }

    #[test]
    fn not_retryable_on_unknown() {
        let err = anyhow::anyhow!("something unexpected");
        assert!(!is_retryable_subgroup_error(&err));
    }

    // --- Retry loop behavior tests ---

    fn make_txn_conflict_error() -> anyhow::Error {
        anyhow::anyhow!(tikv_client::Error::KeyError(Box::new(
            tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }
        )))
    }

    #[tokio::test]
    async fn retry_loop_conflict_then_success() {
        // First attempt: txn_conflict, second attempt: success
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let outcome = retry_subgroup_op(3, |_attempt| {
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(make_txn_conflict_error())
                } else {
                    Ok(42usize)
                }
            }
        })
        .await;

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.result.unwrap(), 42);
        assert_eq!(outcome.attempts_used, 2);
        assert!(outcome.retried);
    }

    #[tokio::test]
    async fn retry_loop_conflict_exhausted() {
        // All attempts: txn_conflict → retries exhaust
        let outcome = retry_subgroup_op(3, |_attempt| async {
            Err::<usize, _>(make_txn_conflict_error())
        })
        .await;

        assert!(outcome.result.is_err());
        assert_eq!(outcome.attempts_used, 3);
        assert!(outcome.retried);
        assert_eq!(
            classify_group_commit_error(&outcome.result.unwrap_err()),
            "execution.txn_conflict"
        );
    }

    #[tokio::test]
    async fn retry_loop_non_retryable_no_retry() {
        // Non-retryable error → no retry, immediate failure
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let outcome = retry_subgroup_op(3, |_attempt| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<usize, _>(anyhow::anyhow!("something unexpected")) }
        })
        .await;

        assert!(outcome.result.is_err());
        assert_eq!(outcome.attempts_used, 1);
        assert!(!outcome.retried);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // --- retry_on_lifecycle_conflict unit tests ---
    //
    // These validate the helper in isolation.  The actual production path
    // (presign_upload_part → load_upload_context under concurrent stale
    // lifecycle) is covered by the #[ignore] integration test
    // `test_presign_upload_part_concurrent_stale_lifecycle_retry` in
    // pagefs/tests.rs, which requires real TiKV + S3.

    fn make_lifecycle_write_conflict() -> anyhow::Error {
        anyhow::anyhow!(tikv_client::Error::KeyError(Box::new(
            tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                ..Default::default()
            }
        )))
    }

    #[tokio::test]
    async fn lifecycle_conflict_retry_then_success() {
        // First call: WriteConflict (lifecycle refresh race).
        // Second call: success (fresh read sees updated lifecycle).
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let result = retry_on_lifecycle_conflict(5, |_attempt| {
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(make_lifecycle_write_conflict())
                } else {
                    Ok("ctx_placeholder")
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ctx_placeholder");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "should succeed on second attempt"
        );
    }

    #[tokio::test]
    async fn lifecycle_conflict_exhausted() {
        // All attempts: WriteConflict → retries exhaust
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let result = retry_on_lifecycle_conflict(3, |_attempt| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<&str, _>(make_lifecycle_write_conflict()) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "should exhaust all attempts"
        );
    }

    #[tokio::test]
    async fn lifecycle_non_retryable_error_not_retried() {
        // Non-WriteConflict error → no retry, immediate failure.
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let result = retry_on_lifecycle_conflict(5, |_attempt| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<&str, _>(anyhow::anyhow!("not a write conflict")) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "should fail on first attempt without retrying"
        );
    }

    #[tokio::test]
    async fn lifecycle_undetermined_outcome_not_retried() {
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let result = retry_on_lifecycle_conflict(5, |_attempt| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                let key_err =
                    tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                        conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
                        ..Default::default()
                    }));
                Err::<&str, _>(anyhow::anyhow!(tikv_client::Error::UndeterminedError(
                    Box::new(key_err)
                )))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "unknown commit outcomes must fail closed without retry"
        );
    }
}
