use super::*;

impl EmbeddedPageFs {
    pub(crate) async fn stat(&self, path: &str) -> Result<Inode> {
        let mut txn = self.begin_read().await?;
        let (_, inode) = resolve_path(&mut txn, path).await?;
        Ok(inode)
    }

    pub(crate) async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<Inode>>> {
        let mut txn = self.begin().await?;
        let mut store = TxnBatchStatStore { txn: &mut txn };
        let result = Ok(resolve_paths_batched(&mut store, paths).await);
        let _ = txn.rollback().await;
        result
    }

    pub(crate) async fn batch_readdir(
        &self,
        paths: &[String],
    ) -> Result<Vec<DirectoryEntriesResult>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut txn = self.begin_read().await?;
        let resolved = {
            let mut store = TxnBatchStatStore { txn: &mut txn };
            resolve_paths_with_ids_batched(&mut store, paths).await
        };

        let mut raw_entries_by_index: Vec<Option<DirectoryEntriesResult>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut dir_request_order = Vec::new();
        let mut dir_inode_ids = Vec::new();

        for (idx, resolved_result) in resolved.into_iter().enumerate() {
            let path = &paths[idx];
            let resolved = match resolved_result {
                Ok(resolved) => resolved,
                Err(err) => {
                    raw_entries_by_index[idx] = Some(Err(err));
                    continue;
                }
            };

            if !resolved.inode.is_directory() {
                raw_entries_by_index[idx] =
                    Some(Err(anyhow!(EmbeddedFsError::not_directory(path))));
                continue;
            }

            dir_request_order.push(idx);
            dir_inode_ids.push(resolved.inode_id);
        }

        let hydrated_dirs = load_directory_entries_batch(&mut txn, &dir_inode_ids).await?;
        if hydrated_dirs.len() != dir_request_order.len() {
            return Err(anyhow!(EmbeddedFsError::internal(&format!(
                "batch_readdir hydrated {} directories for {} directory requests",
                hydrated_dirs.len(),
                dir_request_order.len()
            ))));
        }

        for (idx, dir_entries) in dir_request_order.into_iter().zip(hydrated_dirs) {
            raw_entries_by_index[idx] = Some(Ok(dir_entries));
        }

        Ok(raw_entries_by_index
            .into_iter()
            .map(|entry| entry.expect("batch_readdir must produce an entry for every input path"))
            .collect())
    }

    pub(crate) async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<Option<Result<Vec<u8>>>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut inline_reads = Vec::new();
        let mut pack_reads = Vec::new();
        let mut object_reads = Vec::new();
        let mut total_planned = 0u64;

        let mut txn = self.begin_read().await?;
        let resolved = {
            let mut store = TxnBatchStatStore { txn: &mut txn };
            resolve_paths_batched(&mut store, paths).await
        };

        for (idx, inode_result) in resolved.into_iter().enumerate() {
            let path = &paths[idx];
            let inode = match inode_result {
                Ok(inode) => inode,
                Err(err) => {
                    results[idx] = Some(Err(err));
                    continue;
                }
            };

            if inode.is_directory() {
                results[idx] = Some(Err(anyhow!(EmbeddedFsError::is_directory(path))));
                continue;
            }

            if inode.is_symlink() {
                results[idx] = Some(Err(anyhow!(EmbeddedFsError::InvalidInput(
                    "cannot read symlink as file; use readlink".to_string()
                ))));
                continue;
            }

            if inode.size > max_file_bytes as u64 {
                results[idx] = Some(Err(batch_inline_read_entry_too_large_error(
                    inode.size,
                    max_file_bytes,
                )));
                continue;
            }

            total_planned = total_planned.saturating_add(inode.size);
            let file_len = usize::try_from(inode.size).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "file size exceeds addressable memory"
                ))
            })?;

            match &inode.data {
                DataRef::InlineBlob | DataRef::None => {
                    inline_reads.push(PendingBatchInlineReadInline {
                        result_idx: idx,
                        inode,
                    })
                }
                DataRef::PackEntry {
                    bundle_id,
                    offset,
                    len,
                    ..
                } => pack_reads.push(PendingBatchInlineReadPack {
                    result_idx: idx,
                    bundle_id: *bundle_id,
                    bundle_offset: *offset,
                    len: file_len.min(usize::try_from(*len).map_err(|_| {
                        anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                    })?),
                }),
                DataRef::Object { key, .. } => object_reads.push(PendingBatchInlineReadObject {
                    result_idx: idx,
                    inode_id: inode.id,
                    key: key.clone(),
                    len: file_len,
                }),
                DataRef::StagingPages => {
                    results[idx] = Some(Err(anyhow!(EmbeddedFsError::internal(
                        "published read reached internal staging pages",
                    ))));
                }
            }
        }

        if total_planned > max_total_bytes as u64 {
            return Err(batch_inline_read_payload_too_large_error(
                total_planned,
                max_total_bytes,
            ));
        }

        for pending in inline_reads {
            let len = usize::try_from(pending.inode.size).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "file size exceeds addressable memory"
                ))
            })?;
            let data =
                read_file_range_from_txn(&mut txn, pending.inode.id, &pending.inode, 0, len).await;
            results[pending.result_idx] = Some(data);
        }

        let mut uncached_pack_reads = Vec::new();
        for pending in pack_reads {
            if let Some(bytes) =
                self.bundle_cache
                    .get(pending.bundle_id, pending.bundle_offset, pending.len)
            {
                results[pending.result_idx] = Some(Ok(bytes.to_vec()));
            } else {
                uncached_pack_reads.push(pending);
            }
        }

        let pack_windows =
            plan_batch_inline_read_pack_windows(uncached_pack_reads, max_total_bytes)?;
        let mut manifests_by_bundle = HashMap::new();
        for window in &pack_windows {
            if manifests_by_bundle.contains_key(&window.bundle_id) {
                continue;
            }

            let manifest_result = match load_bundle_manifest(&mut txn, window.bundle_id).await {
                Ok(Some(manifest)) => Ok(manifest),
                Ok(None) => Err(anyhow!(EmbeddedFsError::internal(
                    "bundle manifest missing"
                ))),
                Err(err) => Err(err),
            };
            manifests_by_bundle.insert(window.bundle_id, manifest_result);
        }

        // Pre-fetch append deltas from TiKV for all Object reads (before
        // dropping the TiKV snapshot).
        let mut object_deltas: std::collections::HashMap<usize, Vec<u8>> =
            std::collections::HashMap::new();
        for pending in &object_reads {
            let deltas = read_append_deltas(&mut txn, pending.inode_id).await?;
            if !deltas.is_empty() {
                object_deltas.insert(pending.result_idx, deltas);
            }
        }

        drop(txn);

        let mut external_tasks = tokio::task::JoinSet::new();
        let concurrency = fs9_config().batch_stat_concurrency.max(1);
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let has_external_reads = !pack_windows.is_empty() || !object_reads.is_empty();
        let mut shared_s3_error = None;
        let shared_s3 = if has_external_reads {
            match self.s3_client().await {
                Ok(Some(s3)) => Some(s3),
                Ok(None) => {
                    shared_s3_error =
                        Some(anyhow!(EmbeddedFsError::internal("S3 is not configured")));
                    None
                }
                Err(err) => {
                    shared_s3_error = Some(err);
                    None
                }
            }
        } else {
            None
        };

        if let Some(err) = shared_s3_error.as_ref() {
            for window in &pack_windows {
                for entry in &window.entries {
                    results[entry.result_idx] = Some(Err(clone_fs_error(err)));
                }
            }
            for pending in &object_reads {
                results[pending.result_idx] = Some(Err(clone_fs_error(err)));
            }
        }

        if let Some(s3) = shared_s3 {
            for window in pack_windows {
                match manifests_by_bundle.get(&window.bundle_id) {
                    Some(Ok(manifest)) => {
                        let permit = semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("batch_inline_read semaphore must not be closed");
                        let fs = self.clone();
                        let manifest = manifest.clone();
                        let s3 = s3.clone();
                        external_tasks.spawn(async move {
                            let _permit = permit;
                            fs.read_pack_window_entries_with_s3(s3.as_ref(), &manifest, window)
                                .await
                        });
                    }
                    Some(Err(err)) => {
                        for entry in &window.entries {
                            results[entry.result_idx] = Some(Err(clone_fs_error(err)));
                        }
                    }
                    None => {
                        for entry in &window.entries {
                            results[entry.result_idx] = Some(Err(anyhow!(
                                EmbeddedFsError::internal("bundle manifest plan missing")
                            )));
                        }
                    }
                }
            }

            for pending in object_reads {
                let permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("batch_inline_read semaphore must not be closed");
                let s3 = s3.clone();
                let deltas = object_deltas.remove(&pending.result_idx);
                external_tasks.spawn(async move {
                    let _permit = permit;
                    let result = s3.get_object_bytes(&pending.key).await.map(|bytes| {
                        let mut data = bytes.to_vec();
                        if let Some(delta_bytes) = deltas {
                            data.extend_from_slice(&delta_bytes);
                        }
                        if data.len() > pending.len {
                            data.truncate(pending.len);
                        }
                        data
                    });
                    vec![(pending.result_idx, result)]
                });
            }

            while let Some(join_result) = external_tasks.join_next().await {
                let task_entries = match join_result {
                    Ok(entries) => entries,
                    Err(err) => {
                        return Err(anyhow!("batch_inline_read task failed: {err}"));
                    }
                };
                for (idx, result) in task_entries {
                    results[idx] = Some(result);
                }
            }
        }

        Ok(results
            .into_iter()
            .map(|entry| {
                entry.expect("batch_inline_read must produce a result entry for each input path")
            })
            .collect())
    }

    pub(crate) async fn readdir(&self, path: &str) -> Result<Vec<(String, Inode)>> {
        let mut txn = self.begin_read().await?;
        let (inode_id, inode) = resolve_path(&mut txn, path).await?;
        if !inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(path)));
        }

        let mut entries = load_directory_entries_batch(&mut txn, &[inode_id]).await?;
        Ok(entries.pop().unwrap_or_default())
    }

    pub(crate) async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<PageFsRecursiveReaddirResult> {
        if opts.max_entries == 0 {
            return Ok(PageFsRecursiveReaddirResult {
                entries: Vec::new(),
                truncated: false,
                total_dirs_scanned: 0,
            });
        }

        let normalized = normalize_path(path);
        let mut txn = self.begin_read().await?;
        let (root_inode_id, root_inode) = resolve_path(&mut txn, &normalized).await?;
        if !root_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(path)));
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let mut depth_exhausted = false;
        let mut total_dirs_scanned = 0usize;
        let mut visited_dirs = HashSet::from([root_inode_id]);
        let mut frontier = VecDeque::from([(normalized, root_inode_id, 0usize)]);

        while !frontier.is_empty() {
            if entries.len() >= opts.max_entries {
                truncated = true;
                break;
            }

            let current_depth = frontier
                .front()
                .map(|(_, _, depth)| *depth)
                .expect("frontier must be non-empty while traversing");
            let mut current_level = Vec::new();
            while matches!(frontier.front(), Some((_, _, depth)) if *depth == current_depth) {
                current_level.push(
                    frontier
                        .pop_front()
                        .expect("frontier entry must exist while draining current level"),
                );
            }

            let mut level_dirs = Vec::with_capacity(current_level.len());
            let mut planned_entries = 0usize;
            for (dir_path, dir_inode_id, _) in &current_level {
                let remaining = opts
                    .max_entries
                    .saturating_sub(entries.len().saturating_add(planned_entries));
                if remaining == 0 {
                    truncated = true;
                    break;
                }

                let (dir_entries, dir_truncated) = load_directory_entries_limited(
                    &mut txn,
                    dir_path,
                    *dir_inode_id,
                    remaining,
                    opts.exclude_set.as_deref(),
                )
                .await?;
                total_dirs_scanned = total_dirs_scanned.saturating_add(1);
                planned_entries = planned_entries.saturating_add(dir_entries.len());
                level_dirs.push(dir_entries);

                if dir_truncated {
                    truncated = true;
                    break;
                }
            }

            for ((dir_path, _, dir_depth), dir_entries) in current_level.into_iter().zip(level_dirs)
            {
                for (name, child_inode) in dir_entries {
                    let child_path = if dir_path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir_path}/{name}")
                    };

                    if entries.len() >= opts.max_entries {
                        truncated = true;
                        break;
                    }

                    if child_inode.is_directory() {
                        if dir_depth < opts.max_depth && visited_dirs.insert(child_inode.id) {
                            frontier.push_back((child_path.clone(), child_inode.id, dir_depth + 1));
                        } else if dir_depth >= opts.max_depth {
                            depth_exhausted = true;
                        }
                    }

                    entries.push((child_path, child_inode));
                }

                if truncated {
                    break;
                }
            }

            if truncated {
                break;
            }
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(PageFsRecursiveReaddirResult {
            entries,
            truncated: truncated || depth_exhausted,
            total_dirs_scanned,
        })
    }

    // Plan metadata under a TiKV snapshot, then execute any external object-store reads after the
    // snapshot drops. Inline reads stay fully inside the metadata phase.
    async fn plan_resolved_file_read(
        &self,
        txn: &mut Transaction,
        path: &str,
        inode: Inode,
        max_bytes: Option<usize>,
    ) -> Result<ResolvedFileReadPlan> {
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot read symlink as file; use readlink".to_string()
            )));
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        if let Some(max_bytes) = max_bytes {
            if file_len > max_bytes {
                return Err(anyhow!(
                    "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
                ));
            }
        }

        match &inode.data {
            DataRef::Object { key, .. } => {
                let deltas = read_append_deltas(txn, inode.id).await?;
                Ok(ResolvedFileReadPlan::Object {
                    key: key.clone(),
                    len: file_len,
                    deltas,
                })
            }
            DataRef::PackEntry {
                bundle_id,
                offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_len = usize::try_from(*len).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                })?;
                Ok(ResolvedFileReadPlan::Pack {
                    manifest,
                    bundle_id: *bundle_id,
                    bundle_offset: *offset,
                    len: file_len.min(entry_len),
                })
            }
            _ => Ok(ResolvedFileReadPlan::Ready(
                read_file_range_from_txn(txn, inode.id, &inode, 0, file_len).await?,
            )),
        }
    }

    async fn execute_resolved_file_read_plan(&self, plan: ResolvedFileReadPlan) -> Result<Vec<u8>> {
        match plan {
            ResolvedFileReadPlan::Ready(data) => Ok(data),
            ResolvedFileReadPlan::Object { key, len, deltas } => {
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let bytes = s3.get_object_bytes(&key).await?;
                let mut data = bytes.to_vec();
                if !deltas.is_empty() {
                    data.extend_from_slice(&deltas);
                }
                if data.len() > len {
                    data.truncate(len);
                }
                Ok(data)
            }
            ResolvedFileReadPlan::Pack {
                manifest,
                bundle_id,
                bundle_offset,
                len,
            } => Ok(self
                .read_pack_entry_bytes(&manifest, bundle_id, bundle_offset, 0, len)
                .await?
                .to_vec()),
        }
    }

    async fn plan_file_range_read(
        &self,
        txn: &mut Transaction,
        path: &str,
        inode: Inode,
        offset: u64,
        length: usize,
    ) -> Result<FileRangeReadPlan> {
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot read symlink as file; use readlink".to_string()
            )));
        }

        if offset >= inode.size || length == 0 {
            return Ok(FileRangeReadPlan::Ready(Vec::new()));
        }

        match &inode.data {
            DataRef::Object { key, .. } => {
                let available = inode.size - offset;
                let len = usize::try_from(available.min(length as u64)).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal(
                        "read length exceeds addressable memory"
                    ))
                })?;
                let delta_bytes = read_append_deltas(txn, inode.id).await?;
                let deltas = if delta_bytes.is_empty() {
                    None
                } else {
                    let delta_len = u64::try_from(delta_bytes.len()).unwrap_or(0);
                    let base_size = inode.size.saturating_sub(delta_len);
                    Some((delta_bytes, base_size))
                };
                Ok(FileRangeReadPlan::Object {
                    key: key.clone(),
                    offset,
                    len,
                    deltas,
                })
            }
            DataRef::PackEntry {
                bundle_id,
                offset: bundle_offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_size = u64::from(*len);
                let available = entry_size.saturating_sub(offset);
                let len = usize::try_from(available.min(length as u64)).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal(
                        "pack read length exceeds addressable memory",
                    ))
                })?;
                Ok(FileRangeReadPlan::Pack {
                    manifest,
                    bundle_id: *bundle_id,
                    bundle_offset: *bundle_offset,
                    file_offset: offset,
                    len,
                })
            }
            _ => Ok(FileRangeReadPlan::Ready(
                read_file_range_from_txn(txn, inode.id, &inode, offset, length).await?,
            )),
        }
    }

    async fn execute_file_range_read_plan(&self, plan: FileRangeReadPlan) -> Result<Vec<u8>> {
        match plan {
            FileRangeReadPlan::Ready(data) => Ok(data),
            FileRangeReadPlan::Object {
                key,
                offset,
                len,
                deltas,
            } => {
                if len == 0 {
                    return Ok(Vec::new());
                }
                match deltas {
                    None => {
                        // No deltas — pure S3 range read.
                        let s3 = self.s3_client().await?.ok_or_else(|| {
                            anyhow!(EmbeddedFsError::internal("S3 is not configured"))
                        })?;
                        Ok(s3.get_object_range_bytes(&key, offset, len).await?.to_vec())
                    }
                    Some((delta_bytes, base_size)) => {
                        let read_end = offset + len as u64;
                        let mut result = Vec::with_capacity(len);

                        // Portion from S3 base object.
                        if offset < base_size {
                            let s3 = self.s3_client().await?.ok_or_else(|| {
                                anyhow!(EmbeddedFsError::internal("S3 is not configured"))
                            })?;
                            let s3_len =
                                usize::try_from((base_size - offset).min(len as u64)).unwrap_or(0);
                            let base_bytes =
                                s3.get_object_range_bytes(&key, offset, s3_len).await?;
                            result.extend_from_slice(&base_bytes);
                        }

                        // Portion from TiKV deltas.
                        if read_end > base_size {
                            let delta_start = if offset > base_size {
                                usize::try_from(offset - base_size).unwrap_or(0)
                            } else {
                                0
                            };
                            let delta_end = delta_bytes
                                .len()
                                .min(usize::try_from(read_end - base_size).unwrap_or(usize::MAX));
                            if delta_start < delta_end && delta_start < delta_bytes.len() {
                                result.extend_from_slice(&delta_bytes[delta_start..delta_end]);
                            }
                        }

                        result.truncate(len);
                        Ok(result)
                    }
                }
            }
            FileRangeReadPlan::Pack {
                manifest,
                bundle_id,
                bundle_offset,
                file_offset,
                len,
            } => {
                if len == 0 {
                    return Ok(Vec::new());
                }
                Ok(self
                    .read_pack_entry_bytes(&manifest, bundle_id, bundle_offset, file_offset, len)
                    .await?
                    .to_vec())
            }
        }
    }

    pub(crate) async fn read_file_capped(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_resolved_file_read(&mut txn, path, inode, Some(max_bytes))
                .await?
        };
        self.execute_resolved_file_read_plan(plan).await
    }

    #[cfg(test)]
    pub(crate) async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_resolved_file_read(&mut txn, path, inode, None)
                .await?
        };
        self.execute_resolved_file_read_plan(plan).await
    }

    pub(crate) async fn read_file_at(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_file_range_read(&mut txn, path, inode, offset, length)
                .await?
        };
        self.execute_file_range_read_plan(plan).await
    }

    pub(crate) async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let mut txn = self.begin_read().await?;
        let (inode_id, inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot read symlink as file; use readlink".to_string()
            )));
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        if file_len > max_bytes {
            return Err(anyhow!(
                "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
            ));
        }

        let plan = self.plan_stream_read(txn, inode_id, inode).await?;
        let (tx, rx) = mpsc::channel(8);
        let fs = self.clone();
        let err_sender = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = fs.stream_read_plan_into_channel(plan, tx).await {
                let _ = err_sender
                    .send(Err(std::io::Error::other(err.to_string())))
                    .await;
            }
        });

        Ok(Box::new(ChunkReceiverReader::new(rx)))
    }

    pub(super) async fn cleanup_pending_write_recovery(&self) -> Result<()> {
        self.cleanup_pending_bundle_journals().await?;
        self.cleanup_pending_packing_recovery().await?;
        self.cleanup_marked_orphans().await?;
        self.cleanup_stale_staging_writes().await
    }

    pub(super) async fn cleanup_lifecycle_once(&self) -> Result<()> {
        let s3 = self.s3_client().await?;
        let Some(s3) = s3 else {
            return Ok(());
        };

        let mut txn = self.begin_internal().await?;
        let entries = lifecycle::scan_lifecycle(&mut txn, 1024).await?;
        let _ = txn.rollback().await;

        let now = current_unix_timestamp();
        for (inode_id, state) in entries {
            if state.fs_instance_id() != self.runtime_state().fs_instance_id {
                continue;
            }

            match state {
                FileLifecycle::Deleting {
                    fs_instance_id,
                    data_ref,
                } => {
                    if let DataRef::Object { key, .. } = &data_ref {
                        // External delete is idempotent.
                        if let Err(err) = s3.delete_object(key).await {
                            warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                            continue;
                        }
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Deleting {
                            fs_instance_id,
                            data_ref,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        // Best-effort: if the inode is still around and unpublished, drop it.
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Uploading {
                    fs_instance_id,
                    upload_id,
                    updated_at,
                    reservation,
                } => {
                    if !uploading_lifecycle_is_reapable(now, updated_at, reservation.as_ref()) {
                        continue;
                    }

                    let Some(key) = self.load_staging_object_key(inode_id).await? else {
                        continue;
                    };
                    let mut external_ok = true;
                    if let Some(upload_id) = upload_id.as_deref() {
                        if let Err(err) = s3.abort_multipart_upload(&key, upload_id).await {
                            warn!(
                                "fs9 gc: failed to abort multipart upload for inode {inode_id}: {err}"
                            );
                            external_ok = false;
                        }
                    }
                    if let Err(err) = s3.delete_object(&key).await {
                        warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                        external_ok = false;
                    }
                    if !external_ok {
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Uploading {
                            fs_instance_id,
                            upload_id,
                            updated_at,
                            reservation,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        let _ = clear_staging_write(&mut txn, inode_id).await;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Committing {
                    fs_instance_id,
                    updated_at,
                    reservation,
                } => {
                    if now.saturating_sub(updated_at) < STALE_WRITE_STREAM_SECS {
                        continue;
                    }

                    let Some(key) = self.load_staging_object_key(inode_id).await? else {
                        continue;
                    };
                    if let Err(err) = s3.delete_object(&key).await {
                        warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Committing {
                            fs_instance_id,
                            updated_at,
                            reservation,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        let _ = clear_staging_write(&mut txn, inode_id).await;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Packing {
                    fs_instance_id,
                    bundle_id,
                    updated_at,
                } => {
                    if now.saturating_sub(updated_at) < STALE_PACKING_SECS {
                        continue;
                    }

                    let key = match self.bundle_key(bundle_id) {
                        Ok(key) => key,
                        Err(_) => continue,
                    };
                    let deleted = match s3.delete_object(&key).await {
                        Ok(()) => true,
                        Err(err) => {
                            warn!("fs9: failed to delete stale pack bundle {bundle_id}: {err}");
                            false
                        }
                    };
                    let _ = self.bundle_spool.remove_bundle(bundle_id).await;
                    if !deleted {
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Packing {
                            fs_instance_id,
                            bundle_id,
                            updated_at,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
            }
        }

        let mut txn = self.begin_internal().await?;
        let manifests = scan_bundle_manifests(&mut txn, 1024).await?;
        let _ = txn.rollback().await;

        for manifest in manifests {
            if manifest.fs_instance_id != self.runtime_state().fs_instance_id {
                continue;
            }
            if manifest.needs_compaction() {
                continue;
            }
            if manifest.state != BundleManifestState::PendingDelete {
                continue;
            }

            if let Err(err) = s3.delete_object(&manifest.key).await {
                warn!(
                    "fs9: failed to delete pending-delete bundle {} (key={}): {err}",
                    manifest.bundle_id, manifest.key
                );
                continue;
            }

            let mut txn = self.begin_internal().await?;
            let current = load_bundle_manifest(&mut txn, manifest.bundle_id).await?;
            if current.as_ref() == Some(&manifest) {
                delete_bundle_manifest(&mut txn, manifest.bundle_id).await?;
                txn.commit().await?;
            } else {
                let _ = txn.rollback().await;
            }
        }

        Ok(())
    }
}
