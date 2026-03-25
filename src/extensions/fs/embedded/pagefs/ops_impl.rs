use super::*;

impl EmbeddedPageFs {
    pub(crate) async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
        let normalized = normalize_path(path);
        let (key, storage, size) = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, &normalized).await?;
            if inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::is_directory(&normalized)));
            }

            if inode.is_symlink() {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(
                    "cannot download symlink; use readlink".to_string()
                )));
            }

            let (key, storage) = match &inode.data {
                DataRef::Object { key, .. } => (key.clone(), FsStorage::Object),
                _ => {
                    return Err(anyhow!(EmbeddedFsError::InvalidInput(
                        "prepare_download is only supported for object-backed files".to_string(),
                    )))
                }
            };
            (key, storage, inode.size)
        };

        let ttl_secs = fs9_config().presign_ttl_secs.max(1);
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        let request = s3.presign_get_object(&key, ttl_secs).await?;
        Ok(FsPreparedDownload {
            request,
            size,
            storage,
            range_supported: true,
        })
    }

    pub(crate) async fn write_file_at(
        &self,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut txn = self.begin().await?;
        let prepared = prepare_write_at_file_txn(self, &mut txn, path, None).await?;
        let PreparedFile {
            inode_id,
            mut inode,
            parent_inode,
            is_new,
        } = prepared;
        apply_inline_write_at(
            &mut txn,
            inode_id,
            &mut inode,
            offset,
            data,
            "write_file_at",
            "partial mutation is not supported for sealed files",
            "partial mutation is only supported for inline files up to",
        )
        .await?;
        txn.commit().await?;

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

    pub(crate) async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut txn = self.begin().await?;
        let prepared = prepare_write_at_file_txn(self, &mut txn, path, None).await?;
        let PreparedFile {
            inode_id,
            mut inode,
            parent_inode,
            is_new,
        } = prepared;
        let offset = inode.size;
        apply_inline_write_at(
            &mut txn,
            inode_id,
            &mut inode,
            offset,
            data,
            "append_file",
            "append is not supported for sealed files",
            "append is only supported for inline files up to",
        )
        .await?;
        txn.commit().await?;

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

    pub(crate) async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot truncate symlink; use readlink".to_string()
            )));
        }
        if matches!(
            inode.data,
            DataRef::Object { .. } | DataRef::PackEntry { .. }
        ) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "truncate is not supported for sealed files".to_string()
            )));
        }

        if size == inode.size {
            inode.touch_atime();
            save_inode(&mut txn, &inode).await?;
            txn.commit().await?;
            return Ok(());
        }

        if !can_store_inline_u64(size) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "truncate is only supported for inline files up to {} bytes",
                fs9_config().inline_max_bytes
            ))));
        }

        let mut buf = load_inline_file_buffer(&mut txn, inode_id, &inode, "truncate").await?;
        blob::apply_truncate(&mut buf, size)?;
        blob::write_blob(&mut txn, inode_id, &buf).await?;
        inode.data = DataRef::InlineBlob;
        inode.size = size;
        bump_inode_generation(&mut inode)?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;
        let (parent_inode, _) = resolve_parent(&mut txn, path).await?;
        txn.commit().await?;

        let normalized = normalize_path(path);
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: normalized,
            old_path: None,
            inode: inode_id,
            parent_inode,
            generation: inode.generation,
            is_dir: false,
            size: inode.size,
        });

        Ok(())
    }

    pub(crate) async fn remove(&self, path: &str) -> Result<()> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot remove root".to_string()
            )));
        }

        let mut txn = self.begin().await?;
        let (inode_id, inode) = resolve_path(&mut txn, &normalized).await?;

        if inode.is_directory() {
            let children = list_dir(&mut txn, inode_id).await?;
            if !children.is_empty() {
                return Err(anyhow!(EmbeddedFsError::directory_not_empty(&normalized)));
            }
        } else {
            retire_inode_data_ref(
                &mut txn,
                inode_id,
                &inode.data,
                self.runtime_state().fs_instance_id,
            )
            .await?;
        }

        let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
        let is_dir = inode.is_directory();
        let inode_size = inode.size;
        let inode_generation = inode.generation;
        unlink(&mut txn, parent_inode, &name).await?;
        delete_inode(&mut txn, inode_id).await?;

        txn.commit().await?;

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Delete,
            path: normalized,
            old_path: None,
            inode: inode_id,
            parent_inode,
            generation: inode_generation,
            is_dir,
            size: inode_size,
        });

        Ok(())
    }

    pub(crate) async fn remove_recursive(&self, path: &str) -> Result<u64> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot remove root".to_string()
            )));
        }

        let mut txn = self.begin().await?;
        let (inode_id, inode) = resolve_path(&mut txn, &normalized).await?;
        let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
        let is_dir = inode.is_directory();
        let inode_size = inode.size;
        let inode_generation = inode.generation;

        let removed = remove_inode_recursive(
            &mut txn,
            inode_id,
            inode,
            self.runtime_state().fs_instance_id,
        )
        .await?;
        unlink(&mut txn, parent_inode, &name).await?;

        txn.commit().await?;

        // Phase 1: emit single root DELETE (not per-child).
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Delete,
            path: normalized,
            old_path: None,
            inode: inode_id,
            parent_inode,
            generation: inode_generation,
            is_dir,
            size: inode_size,
        });

        Ok(removed)
    }

    pub(crate) async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Ok(());
        }

        // Collect created directories for event emission after commit.
        struct CreatedDir {
            path: String,
            inode_id: u64,
            parent_inode: u64,
        }

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let mut created_dirs: Vec<CreatedDir> = Vec::new();

            let result: Result<()> = async {
                if !recursive {
                    let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
                    if lookup(&mut txn, parent_inode, &name).await?.is_some() {
                        return Err(anyhow!(EmbeddedFsError::already_exists(&normalized)));
                    }

                    let new_inode_id = self.alloc_inode_id().await?;
                    let inode = Inode::new_directory(new_inode_id, mode.unwrap_or(0o755));
                    save_inode(&mut txn, &inode).await?;
                    link(&mut txn, parent_inode, &name, new_inode_id).await?;
                    created_dirs.push(CreatedDir {
                        path: normalized.clone(),
                        inode_id: new_inode_id,
                        parent_inode,
                    });
                    txn.commit().await?;
                    return Ok(());
                }

                let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
                let mut current_inode = ROOT_INODE;
                let last_idx = parts.len().saturating_sub(1);

                for (i, part) in parts.iter().enumerate() {
                    if let Some(next_inode_id) = lookup(&mut txn, current_inode, part).await? {
                        let next_inode = load_inode(&mut txn, next_inode_id)
                            .await?
                            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&normalized)))?;
                        if !next_inode.is_directory() {
                            return Err(anyhow!(EmbeddedFsError::not_directory(part)));
                        }
                        current_inode = next_inode_id;
                    } else {
                        let new_inode_id = self.alloc_inode_id().await?;
                        // Only the leaf directory gets the requested mode;
                        // intermediate directories always use 0o755.
                        let dir_mode = if i == last_idx {
                            mode.unwrap_or(0o755)
                        } else {
                            0o755
                        };
                        let inode = Inode::new_directory(new_inode_id, dir_mode);
                        save_inode(&mut txn, &inode).await?;
                        link(&mut txn, current_inode, part, new_inode_id).await?;
                        let dir_path = format!("/{}", parts[..=i].join("/"));
                        created_dirs.push(CreatedDir {
                            path: dir_path,
                            inode_id: new_inode_id,
                            parent_inode: current_inode,
                        });
                        current_inode = new_inode_id;
                    }
                }

                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => {
                    // Emit Mkdir events for all created directories.
                    let builders: Vec<FsEventBuilder> = created_dirs
                        .into_iter()
                        .map(|d| FsEventBuilder {
                            event_type: FsEventType::Mkdir,
                            path: d.path,
                            old_path: None,
                            inode: d.inode_id,
                            parent_inode: d.parent_inode,
                            generation: 1,
                            is_dir: true,
                            size: 0,
                        })
                        .collect();
                    self.emit_events(builders);
                    return Ok(());
                }
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal("mkdir retry exhausted")))
    }

    pub(crate) async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old_normalized = normalize_path(old_path);
        let new_normalized = normalize_path(new_path);
        let new_has_trailing_slash = new_path.len() > 1 && new_path.ends_with('/');

        if old_normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot rename root".to_string(),
            )));
        }

        // No-op if paths are identical — but source must exist (POSIX: ENOENT)
        if old_normalized == new_normalized {
            let mut txn = self.begin().await?;
            let (_, inode) = resolve_path(&mut txn, &old_normalized).await?;
            if new_has_trailing_slash && !inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }
            return Ok(());
        }

        let mut txn = self.begin().await?;

        // Resolve the source first — NotFound takes precedence over cycle check
        let (old_inode_id, old_inode) = resolve_path(&mut txn, &old_normalized).await?;
        let (old_parent_inode, old_name) = resolve_parent(&mut txn, &old_normalized).await?;

        // Destination parent must already exist (POSIX semantics — no auto-create)
        let (new_parent_inode, new_name) = resolve_parent(&mut txn, &new_normalized).await?;

        // If destination has trailing slash, source must be a directory (POSIX ENOTDIR).
        if new_has_trailing_slash && !old_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
        }

        // Prevent directory cycle: renaming a dir into its own subtree
        // would corrupt the directory tree (POSIX returns EINVAL for this).
        // Parent existence must be checked first so ENOENT takes precedence.
        // Only applies to directories — files cannot create cycles.
        if old_inode.is_directory() && new_normalized.starts_with(&format!("{old_normalized}/")) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "cannot rename {old_normalized} into its own subdirectory {new_normalized}"
            ))));
        }

        // Check if destination already exists
        if let Some(existing_inode_id) = lookup(&mut txn, new_parent_inode, &new_name).await? {
            let existing_inode = load_inode(&mut txn, existing_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&new_normalized)))?;

            if new_has_trailing_slash && !existing_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }

            if existing_inode.is_directory() {
                // Don't replace existing directories
                return Err(anyhow!(EmbeddedFsError::already_exists(&new_normalized)));
            }

            if old_inode.is_directory() {
                // Can't overwrite a file with a directory
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }

            // Source is file, dest is file: replace (delete dest's data)
            retire_inode_data_ref(
                &mut txn,
                existing_inode_id,
                &existing_inode.data,
                self.runtime_state().fs_instance_id,
            )
            .await?;
            delete_inode(&mut txn, existing_inode_id).await?;
            unlink(&mut txn, new_parent_inode, &new_name).await?;
        }

        // Unlink from old parent, link to new parent
        unlink(&mut txn, old_parent_inode, &old_name).await?;
        link(&mut txn, new_parent_inode, &new_name, old_inode_id).await?;

        let is_dir = old_inode.is_directory();
        let inode_generation = old_inode.generation;
        let inode_size = old_inode.size;

        txn.commit().await?;

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Rename,
            path: new_normalized,
            old_path: Some(old_normalized),
            inode: old_inode_id,
            parent_inode: new_parent_inode,
            generation: inode_generation,
            is_dir,
            size: inode_size,
        });

        Ok(())
    }

    pub(super) async fn plan_stream_read(
        &self,
        mut txn: Transaction,
        inode_id: u64,
        inode: Inode,
    ) -> Result<StreamReadPlan> {
        match &inode.data {
            DataRef::Object { key, .. } => Ok(StreamReadPlan::Object { key: key.clone() }),
            DataRef::PackEntry {
                bundle_id,
                offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(&mut txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_len = usize::try_from(*len).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                })?;
                Ok(StreamReadPlan::Pack {
                    manifest,
                    bundle_offset: *offset,
                    len: entry_len.min(usize::try_from(inode.size).unwrap_or(entry_len)),
                })
            }
            _ => Ok(StreamReadPlan::Inline {
                txn: Box::new(txn),
                inode_id,
                inode,
            }),
        }
    }

    pub(super) async fn stream_read_plan_into_channel(
        &self,
        plan: StreamReadPlan,
        sender: mpsc::Sender<std::io::Result<Vec<u8>>>,
    ) -> Result<()> {
        match plan {
            StreamReadPlan::Object { key } => {
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let mut reader = s3.get_object_stream(&key).await?;
                let mut buf = vec![0u8; STREAM_READ_CHUNK_BYTES];
                loop {
                    let n = reader.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    if sender.send(Ok(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
            StreamReadPlan::Pack {
                manifest,
                bundle_offset,
                len,
            } => {
                if len == 0 {
                    return Ok(());
                }
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let mut reader = s3
                    .get_object_range_stream(&manifest.key, bundle_offset, len)
                    .await?;
                let mut remaining = len;
                let mut buf = vec![0u8; STREAM_READ_CHUNK_BYTES];
                while remaining > 0 {
                    if sender.is_closed() {
                        break;
                    }
                    let n = reader
                        .read(&mut buf[..STREAM_READ_CHUNK_BYTES.min(remaining)])
                        .await?;
                    if n == 0 {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "pack entry range read ended early"
                        )));
                    }
                    remaining = remaining.saturating_sub(n);
                    if sender.send(Ok(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
            StreamReadPlan::Inline {
                mut txn,
                inode_id,
                inode,
            } => {
                let mut offset = 0u64;
                let file_size = inode.size;
                while offset < file_size {
                    let remaining = file_size - offset;
                    let chunk_len = usize::try_from(remaining.min(STREAM_READ_CHUNK_BYTES as u64))
                        .map_err(|_| {
                            anyhow!(EmbeddedFsError::internal("stream chunk exceeds usize"))
                        })?;
                    let chunk =
                        read_file_range_from_txn(&mut txn, inode_id, &inode, offset, chunk_len)
                            .await?;

                    if sender.send(Ok(chunk)).await.is_err() {
                        break;
                    }

                    offset = offset.checked_add(chunk_len as u64).ok_or_else(|| {
                        anyhow!(EmbeddedFsError::internal("stream offset overflow"))
                    })?;
                }

                Ok(())
            }
        }
    }

    pub(super) async fn flush_staged_write_chunk(
        &self,
        staging_inode_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let mut txn = self.begin().await?;
        let mut inode = load_inode(&mut txn, staging_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
        if inode.nlink != 0 {
            return Err(anyhow!(EmbeddedFsError::internal(
                "staging inode already published"
            )));
        }

        match inode.data {
            DataRef::None | DataRef::StagingPages => {
                inode.data = DataRef::StagingPages;
            }
            _ => {
                return Err(anyhow!(EmbeddedFsError::internal(
                    "staging inode has unexpected non-paged data ref"
                )))
            }
        }

        write_staging_chunk_to_txn(&mut txn, staging_inode_id, &mut inode, offset, data).await?;
        save_inode(&mut txn, &inode).await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    pub(super) async fn touch_staging_write(&self, staging_inode_id: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    pub(super) async fn publish_staged_write(
        &self,
        path: &str,
        staging_inode_id: u64,
    ) -> Result<usize> {
        self.publish_staged_write_with_reservation(path, staging_inode_id, None)
            .await
    }

    pub(super) async fn publish_staged_write_with_reservation(
        &self,
        path: &str,
        staging_inode_id: u64,
        reservation: Option<&UploadReservation>,
    ) -> Result<usize> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            match self
                .publish_staged_write_once(path, staging_inode_id, reservation)
                .await
            {
                Ok((size, orphan_inode_id)) => {
                    if let Some(inode_id) = orphan_inode_id {
                        self.spawn_orphan_cleanup(inode_id);
                    }
                    return Ok(size);
                }
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "publish staged write retry exhausted"
        )))
    }

    async fn publish_staged_write_once(
        &self,
        path: &str,
        staging_inode_id: u64,
        reservation: Option<&UploadReservation>,
    ) -> Result<(usize, Option<u64>)> {
        let mut txn = self.begin().await?;
        if let Some(reservation) = reservation {
            verify_upload_publish_preconditions(&mut txn, path, reservation).await?;
        }
        let (parent_inode, name) = ensure_parents_and_resolve_parent(self, &mut txn, path).await?;

        let mut staging_inode = load_inode(&mut txn, staging_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
        if staging_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        // Publish should advance the per-path generation counter so CAS can detect
        // intervening in-place mutations even when inode ids are stable.
        let mut publish_generation = staging_inode.generation.max(1);
        let mut is_overwrite = false;
        let orphan_inode_id =
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let mut existing_inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;
                if existing_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(path)));
                }

                if existing_inode.is_symlink() {
                    return Err(anyhow!(EmbeddedFsError::InvalidInput(
                        "cannot write to symlink as file; use readlink".to_string()
                    )));
                }

                is_overwrite = true;
                publish_generation = existing_inode.generation.checked_add(1).ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal("inode generation overflow"))
                })?;
                // Preserve existing file's mode on overwrite (new-inode-only semantics).
                staging_inode.mode = existing_inode.mode;
                existing_inode.nlink = 0;
                save_inode(&mut txn, &existing_inode).await?;
                match &existing_inode.data {
                    DataRef::Object { .. } => {
                        retire_inode_data_ref(
                            &mut txn,
                            existing_inode_id,
                            &existing_inode.data,
                            self.runtime_state().fs_instance_id,
                        )
                        .await?;
                        None
                    }
                    DataRef::PackEntry { .. } => {
                        retire_inode_data_ref(
                            &mut txn,
                            existing_inode_id,
                            &existing_inode.data,
                            self.runtime_state().fs_instance_id,
                        )
                        .await?;
                        delete_inode(&mut txn, existing_inode_id).await?;
                        None
                    }
                    DataRef::None | DataRef::InlineBlob => {
                        mark_orphan_inode(&mut txn, existing_inode_id).await?;
                        Some(existing_inode_id)
                    }
                    DataRef::StagingPages => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "published file cannot use staging pages during overwrite",
                        )))
                    }
                }
            } else {
                None
            };

        staging_inode.generation = publish_generation;
        staging_inode.nlink = 1;
        staging_inode.touch_mtime();
        save_inode(&mut txn, &staging_inode).await?;
        link(&mut txn, parent_inode, &name, staging_inode_id).await?;
        lifecycle::clear_lifecycle(&mut txn, staging_inode_id).await?;
        clear_staging_write(&mut txn, staging_inode_id).await?;
        let emit_size = staging_inode.size;
        let emit_generation = staging_inode.generation;
        txn.commit().await?;

        let normalized = normalize_path(path);
        self.emit_event(FsEventBuilder {
            event_type: if is_overwrite {
                FsEventType::Write
            } else {
                FsEventType::Create
            },
            path: normalized,
            old_path: None,
            inode: staging_inode_id,
            parent_inode,
            generation: emit_generation,
            is_dir: false,
            size: emit_size,
        });

        Ok((
            usize::try_from(emit_size)
                .map_err(|_| anyhow!(EmbeddedFsError::internal("staging size exceeds usize")))?,
            orphan_inode_id,
        ))
    }

    pub(super) async fn abort_staged_write(&self, staging_inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        let inode = load_inode(&mut txn, staging_inode_id).await?;
        let lifecycle_state = lifecycle::load_lifecycle(&mut txn, staging_inode_id).await?;
        let _ = txn.rollback().await;

        // Guard: if the inode has been published (nlink > 0), it is live data.
        // This can happen when publish_staged_write commits in TiKV but the
        // client observes a timeout and triggers abort. Deleting it would
        // corrupt the published file.
        if inode.as_ref().is_some_and(|inode| inode.nlink != 0) {
            return Ok(());
        }

        let Some(expected) = lifecycle_state else {
            return self.cleanup_staging_inode(staging_inode_id).await;
        };

        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        let object_key = || -> Result<String> {
            if let Some(inode) = inode.as_ref() {
                if let DataRef::Object { key, .. } = &inode.data {
                    return Ok(key.clone());
                }
            }
            self.object_key(staging_inode_id)
        };

        let mut external_ok = true;
        match &expected {
            FileLifecycle::Deleting { data_ref, .. } => {
                if let DataRef::Object { key, .. } = data_ref {
                    if let Err(err) = s3.delete_object(key).await {
                        warn!(
                            "fs9 abort: failed to delete object for inode {staging_inode_id}: {err}"
                        );
                        external_ok = false;
                    }
                }
            }
            FileLifecycle::Uploading { upload_id, .. } => {
                let key = object_key()?;
                if let Some(upload_id) = upload_id.as_deref() {
                    if let Err(err) = s3.abort_multipart_upload(&key, upload_id).await {
                        warn!(
                            "fs9 abort: failed to abort multipart upload for inode {staging_inode_id}: {err}"
                        );
                        external_ok = false;
                    }
                }
                if let Err(err) = s3.delete_object(&key).await {
                    warn!("fs9 abort: failed to delete object for inode {staging_inode_id}: {err}");
                    external_ok = false;
                }
            }
            FileLifecycle::Committing { .. } => {
                let key = object_key()?;
                if let Err(err) = s3.delete_object(&key).await {
                    warn!("fs9 abort: failed to delete object for inode {staging_inode_id}: {err}");
                    external_ok = false;
                }
            }
            FileLifecycle::Packing { .. } => {
                // Packing is not implemented in M4; preserve lifecycle state for future GC.
                external_ok = false;
            }
        }

        if !external_ok {
            self.mark_lifecycle_stale_for_retry(staging_inode_id, &expected)
                .await?;
            return Ok(());
        }

        let mut txn = self.begin_internal().await?;
        let current = lifecycle::load_lifecycle(&mut txn, staging_inode_id).await?;
        if current.as_ref() == Some(&expected) {
            lifecycle::clear_lifecycle(&mut txn, staging_inode_id).await?;
            clear_staging_write(&mut txn, staging_inode_id).await?;
            if let Some(inode) = load_inode(&mut txn, staging_inode_id).await? {
                if inode.nlink == 0 && !inode.is_directory() {
                    delete_inode(&mut txn, staging_inode_id).await?;
                }
            }
            txn.commit().await?;
        } else {
            let _ = txn.rollback().await;
        }

        Ok(())
    }

    async fn mark_lifecycle_stale_for_retry(
        &self,
        inode_id: u64,
        expected: &FileLifecycle,
    ) -> Result<()> {
        let stale = match expected {
            FileLifecycle::Uploading {
                fs_instance_id,
                upload_id,
                updated_at: _,
                reservation,
            } => FileLifecycle::Uploading {
                fs_instance_id: *fs_instance_id,
                upload_id: upload_id.clone(),
                updated_at: 0,
                reservation: reservation.clone(),
            },
            FileLifecycle::Committing {
                fs_instance_id,
                updated_at: _,
                reservation,
            } => FileLifecycle::Committing {
                fs_instance_id: *fs_instance_id,
                updated_at: 0,
                reservation: reservation.clone(),
            },
            FileLifecycle::Packing {
                fs_instance_id,
                bundle_id,
                updated_at: _,
            } => FileLifecycle::Packing {
                fs_instance_id: *fs_instance_id,
                bundle_id: *bundle_id,
                updated_at: 0,
            },
            FileLifecycle::Deleting { .. } => return Ok(()),
        };

        let mut txn = self.begin_internal().await?;
        let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
        if current.as_ref() == Some(expected) {
            lifecycle::save_lifecycle(&mut txn, inode_id, &stale).await?;
            txn.commit().await?;
        } else {
            let _ = txn.rollback().await;
        }
        Ok(())
    }

    pub(super) async fn cleanup_marked_orphans(&self) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        let orphan_inode_ids = list_orphan_inodes(&mut txn).await?;
        let _ = txn.rollback().await;

        for inode_id in orphan_inode_ids {
            self.cleanup_orphan_inode(inode_id).await?;
        }
        Ok(())
    }

    pub(super) async fn cleanup_stale_staging_writes(&self) -> Result<()> {
        let cutoff = current_unix_timestamp().saturating_sub(STALE_WRITE_STREAM_SECS);
        let mut txn = self.begin_internal().await?;
        let staging_inode_ids = list_stale_staging_writes(&mut txn, cutoff).await?;
        let _ = txn.rollback().await;

        for inode_id in staging_inode_ids {
            self.cleanup_staging_inode(inode_id).await?;
        }
        Ok(())
    }

    pub(super) async fn cleanup_staging_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
        clear_staging_write(&mut txn, inode_id).await?;

        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
            // Guard: if the inode has been published (nlink > 0), it is live data.
            // This can happen when publish_staged_write commits in TiKV but the
            // client observes a timeout and triggers abort. Deleting it would
            // corrupt the published file.
            if inode.nlink != 0 {
                let _ = txn.rollback().await;
                return Ok(());
            }
            if !inode.is_directory() {
                match inode.data {
                    DataRef::None => {}
                    DataRef::InlineBlob => blob::delete_blob(&mut txn, inode_id).await?,
                    DataRef::Object { .. } => {
                        // Preserve a durable lifecycle marker so object cleanup remains retryable.
                        lifecycle::save_lifecycle(
                            &mut txn,
                            inode_id,
                            &FileLifecycle::Deleting {
                                fs_instance_id: self.runtime_state().fs_instance_id,
                                data_ref: inode.data.clone(),
                            },
                        )
                        .await?;
                    }
                    DataRef::StagingPages => delete_staging_pages(&mut txn, inode_id).await?,
                    other => {
                        return Err(anyhow!(EmbeddedFsError::internal(&format!(
                            "staging cleanup not implemented for data ref {other:?}"
                        ))));
                    }
                }
            }
            delete_inode(&mut txn, inode_id).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    async fn cleanup_orphan_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        clear_orphan_inode(&mut txn, inode_id).await?;

        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
            if !inode.is_directory() {
                match inode.data {
                    DataRef::None => {}
                    DataRef::InlineBlob => blob::delete_blob(&mut txn, inode_id).await?,
                    DataRef::Object { .. } => {}
                    DataRef::StagingPages => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "published orphan inode cannot use staging pages",
                        )))
                    }
                    other => {
                        return Err(anyhow!(EmbeddedFsError::internal(&format!(
                            "orphan cleanup not implemented for data ref {other:?}"
                        ))));
                    }
                }
            }
            delete_inode(&mut txn, inode_id).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    fn spawn_orphan_cleanup(&self, inode_id: u64) {
        let fs = self.clone();
        tokio::spawn(async move {
            if let Err(err) = fs.cleanup_orphan_inode(inode_id).await {
                warn!("embedded fs orphan cleanup failed for inode {inode_id}: {err}");
            }
        });
    }

    pub(crate) async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        if target.is_empty() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "symlink target must not be empty".to_string()
            )));
        }
        if target.contains('\0') {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "symlink target must not contain NUL bytes".to_string()
            )));
        }
        if target.len() > MAX_SYMLINK_TARGET_BYTES {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "symlink target exceeds maximum length of {} bytes",
                MAX_SYMLINK_TARGET_BYTES
            ))));
        }

        let normalized = normalize_path(path);
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let mut did_bump_version = false;
            let mut symlink_inode_id = 0u64;
            let mut symlink_parent_inode = 0u64;
            let result: Result<()> = async {
                // Lazy format version bump: ensure this keyspace declares symlink support.
                // One-time per keyspace; concurrent bumps are resolved by TiKV write conflict + retry.
                let sb = load_current_superblock_if_present(&mut txn)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("superblock missing")))?;
                if sb.format_version < FS9_FORMAT_VERSION_SYMLINK {
                    let mut bumped = sb;
                    bumped.format_version = FS9_FORMAT_VERSION_SYMLINK;
                    save_superblock(&mut txn, &bumped).await?;
                    did_bump_version = true;
                }

                let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
                if lookup(&mut txn, parent_inode, &name).await?.is_some() {
                    return Err(anyhow!(EmbeddedFsError::already_exists(&normalized)));
                }

                let new_inode_id = self.alloc_inode_id().await?;
                let inode = Inode::new_symlink(new_inode_id, 0o777, target.len() as u64);
                save_inode(&mut txn, &inode).await?;
                txn.put(keys::blob_key(new_inode_id), target.as_bytes().to_vec())
                    .await?;
                link(&mut txn, parent_inode, &name, new_inode_id).await?;
                symlink_inode_id = new_inode_id;
                symlink_parent_inode = parent_inode;
                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => {
                    if did_bump_version {
                        tracing::info!(
                            keyspace = %self.keyspace,
                            "fs9: superblock format version bumped to v{} for symlink support",
                            FS9_FORMAT_VERSION_SYMLINK
                        );
                    }
                    self.emit_event(FsEventBuilder {
                        event_type: FsEventType::Create,
                        path: normalized,
                        old_path: None,
                        inode: symlink_inode_id,
                        parent_inode: symlink_parent_inode,
                        generation: 1,
                        is_dir: false,
                        size: target.len() as u64,
                    });
                    return Ok(());
                }
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "symlink retry exhausted"
        )))
    }

    pub(crate) async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let normalized = normalize_path(path);
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let mut chmod_inode_id = 0u64;
            let mut chmod_generation = 0u64;
            let mut chmod_is_dir = false;
            let mut chmod_size = 0u64;
            let mut chmod_parent_inode = 0u64;
            let result: Result<()> = async {
                let (inode_id, mut inode) = resolve_path(&mut txn, &normalized).await?;
                let (parent_inode, _) = resolve_parent(&mut txn, &normalized).await?;
                inode.mode = mode;
                inode.touch_mtime();
                chmod_inode_id = inode_id;
                chmod_generation = inode.generation;
                chmod_is_dir = inode.is_directory();
                chmod_size = inode.size;
                chmod_parent_inode = parent_inode;
                save_inode(&mut txn, &inode).await?;
                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => {
                    self.emit_event(FsEventBuilder {
                        event_type: FsEventType::Write,
                        path: normalized,
                        old_path: None,
                        inode: chmod_inode_id,
                        parent_inode: chmod_parent_inode,
                        generation: chmod_generation,
                        is_dir: chmod_is_dir,
                        size: chmod_size,
                    });
                    return Ok(());
                }
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal("chmod retry exhausted")))
    }

    pub(crate) async fn readlink(&self, path: &str) -> Result<String> {
        let mut txn = self.begin_read().await?;
        let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
        if !inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "not a symlink: {path}"
            ))));
        }
        let blob = txn
            .get(keys::blob_key(inode.id))
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("symlink target blob missing")))?;
        String::from_utf8(blob).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "symlink target is not valid UTF-8"
            ))
        })
    }
}
