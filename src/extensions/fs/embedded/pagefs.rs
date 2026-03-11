use crate::extensions::fs::backend::FsWriteStream;
use crate::extensions::fs::channel_reader::ChunkReceiverReader;
use crate::extensions::fs::embedded::keys;
use crate::extensions::fs::embedded::types::*;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tikv_client::{CheckLevel, Transaction, TransactionClient, TransactionOptions};
use tokio::io::AsyncBufRead;
use tokio::sync::mpsc;
use tracing::warn;

#[derive(Clone)]
pub(crate) struct EmbeddedPageFs {
    client: Arc<TransactionClient>,
}

const STREAM_READ_CHUNK_BYTES: usize = 64 * 1024;
const WRITE_STREAM_FLUSH_BYTES: usize = PAGE_SIZE * 16;
const STALE_WRITE_STREAM_SECS: i64 = 60 * 60;
const STAGING_REFRESH_INTERVAL_SECS: i64 = 5 * 60;

struct EmbeddedPageWriteStream {
    fs: EmbeddedPageFs,
    path: String,
    staging_inode_id: u64,
    buffered: Vec<u8>,
    committed_bytes: u64,
    last_staging_refresh: i64,
}

impl EmbeddedPageWriteStream {
    async fn flush_buffer(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }

        self.fs
            .flush_staged_write_chunk(self.staging_inode_id, self.committed_bytes, &self.buffered)
            .await?;
        self.committed_bytes = self
            .committed_bytes
            .checked_add(self.buffered.len() as u64)
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("stream write overflow")))?;
        self.buffered.clear();
        self.last_staging_refresh = current_unix_timestamp();
        Ok(())
    }

    async fn maybe_refresh_staging_marker(&mut self) -> Result<()> {
        let now = current_unix_timestamp();
        if now - self.last_staging_refresh >= STAGING_REFRESH_INTERVAL_SECS {
            self.fs.touch_staging_write(self.staging_inode_id).await?;
            self.last_staging_refresh = now;
        }
        Ok(())
    }
}

#[async_trait]
impl FsWriteStream for EmbeddedPageWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        self.buffered.extend_from_slice(chunk);
        if self.buffered.len() >= WRITE_STREAM_FLUSH_BYTES {
            self.flush_buffer().await?;
        } else {
            self.maybe_refresh_staging_marker().await?;
        }
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<usize> {
        if let Err(err) = self.flush_buffer().await {
            let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
            return Err(err);
        }

        match self
            .fs
            .publish_staged_write(&self.path, self.staging_inode_id)
            .await
        {
            Ok(written) => Ok(written),
            Err(err) => {
                let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
                Err(err)
            }
        }
    }

    async fn abort(self: Box<Self>) -> Result<()> {
        let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
        Ok(())
    }
}

impl EmbeddedPageFs {
    pub(crate) fn new(client: Arc<TransactionClient>) -> Self {
        Self { client }
    }

    async fn begin(&self) -> Result<Transaction> {
        let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
        self.client
            .begin_with_options(options)
            .await
            .map_err(|e| anyhow!(e))
    }

    pub(crate) async fn init_filesystem(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let superblock_exists = txn.get(keys::superblock_key()).await?.is_some();

        if !superblock_exists {
            let sb = Superblock::default();
            save_superblock(&mut txn, &sb).await?;

            let root = Inode::new_directory(ROOT_INODE, 0o755);
            save_inode(&mut txn, &root).await?;

            txn.commit().await?;
        } else if load_inode(&mut txn, ROOT_INODE).await?.is_none() {
            let root = Inode::new_directory(ROOT_INODE, 0o755);
            save_inode(&mut txn, &root).await?;
            txn.commit().await?;
        } else {
            let _ = txn.rollback().await;
        }

        if let Err(err) = self.cleanup_pending_write_recovery().await {
            warn!("embedded fs recovery cleanup failed: {err}");
        }
        Ok(())
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<Inode> {
        let mut txn = self.begin().await?;
        let (_, inode) = resolve_path(&mut txn, path).await?;
        let _ = txn.rollback().await;
        Ok(inode)
    }

    pub(crate) async fn readdir(&self, path: &str) -> Result<Vec<(String, Inode)>> {
        let mut txn = self.begin().await?;
        let (inode_id, inode) = resolve_path(&mut txn, path).await?;
        if !inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(path)));
        }

        let entries = list_dir(&mut txn, inode_id).await?;
        let mut out = Vec::with_capacity(entries.len());
        for (name, child_inode_id) in entries {
            if let Some(child_inode) = load_inode(&mut txn, child_inode_id).await? {
                out.push((name, child_inode));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));

        let _ = txn.rollback().await;
        Ok(out)
    }

    pub(crate) async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        let data = read_file_range_from_txn(&mut txn, inode_id, inode.size, 0, file_len).await?;

        inode.touch_atime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
        Ok(data)
    }

    pub(crate) async fn read_file_at(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        if offset >= inode.size || length == 0 {
            inode.touch_atime();
            save_inode(&mut txn, &inode).await?;
            txn.commit().await?;
            return Ok(Vec::new());
        }

        let data = read_file_range_from_txn(&mut txn, inode_id, inode.size, offset, length).await?;

        inode.touch_atime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
        Ok(data)
    }

    pub(crate) async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let inode = self.stat(path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
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

        let (tx, rx) = mpsc::channel(8);
        let stream_path = path.to_string();
        let fs = self.clone();
        let err_sender = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = fs.stream_file_into_channel(stream_path, tx).await {
                let _ = err_sender
                    .send(Err(std::io::Error::other(err.to_string())))
                    .await;
            }
        });

        Ok(Box::new(ChunkReceiverReader::new(rx)))
    }

    async fn cleanup_pending_write_recovery(&self) -> Result<()> {
        self.cleanup_marked_orphans().await?;
        self.cleanup_stale_staging_writes().await
    }

    pub(crate) async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = prepare_replace_file_txn(&mut txn, path).await?;
        write_file_chunk_to_txn(&mut txn, inode_id, &mut inode, 0, data).await?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;

        txn.commit().await?;
        Ok(data.len())
    }

    pub(crate) async fn begin_write_stream(&self, path: &str) -> Result<Box<dyn FsWriteStream>> {
        let mut txn = self.begin().await?;
        match resolve_path(&mut txn, path).await {
            Ok((_, inode)) if inode.is_directory() => {
                return Err(anyhow!(EmbeddedFsError::is_directory(path)));
            }
            Ok(_) => {}
            Err(err) if !is_not_found_error(&err) => return Err(err),
            Err(_) => {}
        }

        let inode_id = alloc_inode(&mut txn).await?;
        let mut inode = Inode::new_file(inode_id, 0o644);
        inode.nlink = 0;
        save_inode(&mut txn, &inode).await?;
        mark_staging_write(&mut txn, inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;

        Ok(Box::new(EmbeddedPageWriteStream {
            fs: self.clone(),
            path: normalize_path(path),
            staging_inode_id: inode_id,
            buffered: Vec::with_capacity(WRITE_STREAM_FLUSH_BYTES),
            committed_bytes: 0,
            last_staging_refresh: current_unix_timestamp(),
        }))
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
        let (inode_id, mut inode) = prepare_write_at_file_txn(&mut txn, path).await?;
        write_file_chunk_to_txn(&mut txn, inode_id, &mut inode, offset, data).await?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;

        txn.commit().await?;
        Ok(data.len())
    }

    pub(crate) async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        let mut txn = self.begin().await?;
        ensure_parents(&mut txn, path).await?;
        let (parent_inode, name) = resolve_parent(&mut txn, path).await?;

        let current_size =
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;
                if inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(path)));
                }
                inode.size
            } else {
                let new_inode_id = alloc_inode(&mut txn).await?;
                let inode = Inode::new_file(new_inode_id, 0o644);
                save_inode(&mut txn, &inode).await?;
                link(&mut txn, parent_inode, &name, new_inode_id).await?;
                0
            };

        txn.commit().await?;
        self.write_file_at(path, current_size, data).await
    }

    pub(crate) async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        if size == inode.size {
            inode.touch_atime();
            save_inode(&mut txn, &inode).await?;
            txn.commit().await?;
            return Ok(());
        }

        let new_page_count = pages_needed(size);
        if size > inode.size {
            inode.size = size;
            inode.page_count = new_page_count;
            inode.touch_mtime();
            save_inode(&mut txn, &inode).await?;
            txn.commit().await?;
            return Ok(());
        }

        for page_num in new_page_count..inode.page_count {
            txn.delete(keys::page_key(inode_id, page_num)).await?;
        }

        let tail_offset = (size % PAGE_SIZE as u64) as usize;
        if new_page_count > 0 && tail_offset != 0 {
            let last_page_num = new_page_count - 1;
            if let Some(mut page_data) = read_page(&mut txn, inode_id, last_page_num).await? {
                if page_data.len() < PAGE_SIZE {
                    page_data.resize(PAGE_SIZE, 0);
                }
                page_data[tail_offset..].fill(0);
                write_page(&mut txn, inode_id, last_page_num, &page_data).await?;
            }
        }

        inode.size = size;
        inode.page_count = new_page_count;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
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
            delete_pages(&mut txn, inode_id).await?;
        }

        let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
        unlink(&mut txn, parent_inode, &name).await?;
        delete_inode(&mut txn, inode_id).await?;

        txn.commit().await?;
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

        let removed = remove_inode_recursive(&mut txn, inode_id, inode).await?;
        unlink(&mut txn, parent_inode, &name).await?;

        txn.commit().await?;
        Ok(removed)
    }

    pub(crate) async fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Ok(());
        }

        let mut txn = self.begin().await?;

        if !recursive {
            let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
            if lookup(&mut txn, parent_inode, &name).await?.is_some() {
                return Err(anyhow!(EmbeddedFsError::already_exists(&normalized)));
            }

            let new_inode_id = alloc_inode(&mut txn).await?;
            let inode = Inode::new_directory(new_inode_id, 0o755);
            save_inode(&mut txn, &inode).await?;
            link(&mut txn, parent_inode, &name, new_inode_id).await?;
            txn.commit().await?;
            return Ok(());
        }

        let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_inode = ROOT_INODE;

        for part in parts {
            if let Some(next_inode_id) = lookup(&mut txn, current_inode, part).await? {
                let next_inode = load_inode(&mut txn, next_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&normalized)))?;
                if !next_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::not_directory(part)));
                }
                current_inode = next_inode_id;
            } else {
                let new_inode_id = alloc_inode(&mut txn).await?;
                let inode = Inode::new_directory(new_inode_id, 0o755);
                save_inode(&mut txn, &inode).await?;
                link(&mut txn, current_inode, part, new_inode_id).await?;
                current_inode = new_inode_id;
            }
        }

        txn.commit().await?;
        Ok(())
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
            delete_pages(&mut txn, existing_inode_id).await?;
            delete_inode(&mut txn, existing_inode_id).await?;
            unlink(&mut txn, new_parent_inode, &new_name).await?;
        }

        // Unlink from old parent, link to new parent
        unlink(&mut txn, old_parent_inode, &old_name).await?;
        link(&mut txn, new_parent_inode, &new_name, old_inode_id).await?;

        txn.commit().await?;
        Ok(())
    }

    async fn stream_file_into_channel(
        &self,
        path: String,
        sender: mpsc::Sender<std::io::Result<Vec<u8>>>,
    ) -> Result<()> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, &path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(&path)));
        }

        let mut offset = 0u64;
        let file_size = inode.size;
        while offset < file_size {
            let remaining = file_size - offset;
            let chunk_len = usize::try_from(remaining.min(STREAM_READ_CHUNK_BYTES as u64))
                .map_err(|_| anyhow!(EmbeddedFsError::internal("stream chunk exceeds usize")))?;
            let chunk =
                read_file_range_from_txn(&mut txn, inode_id, file_size, offset, chunk_len).await?;

            if sender.send(Ok(chunk)).await.is_err() {
                break;
            }

            offset = offset
                .checked_add(chunk_len as u64)
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("stream offset overflow")))?;
        }

        inode.touch_atime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn flush_staged_write_chunk(
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

        write_file_chunk_to_txn(&mut txn, staging_inode_id, &mut inode, offset, data).await?;
        save_inode(&mut txn, &inode).await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn touch_staging_write(&self, staging_inode_id: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn publish_staged_write(&self, path: &str, staging_inode_id: u64) -> Result<usize> {
        let mut txn = self.begin().await?;
        ensure_parents(&mut txn, path).await?;
        let (parent_inode, name) = resolve_parent(&mut txn, path).await?;

        let mut staging_inode = load_inode(&mut txn, staging_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
        if staging_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        let orphan_inode_id =
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let mut existing_inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;
                if existing_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(path)));
                }

                existing_inode.nlink = 0;
                save_inode(&mut txn, &existing_inode).await?;
                mark_orphan_inode(&mut txn, existing_inode_id).await?;
                Some(existing_inode_id)
            } else {
                None
            };

        staging_inode.nlink = 1;
        staging_inode.touch_mtime();
        save_inode(&mut txn, &staging_inode).await?;
        link(&mut txn, parent_inode, &name, staging_inode_id).await?;
        clear_staging_write(&mut txn, staging_inode_id).await?;
        txn.commit().await?;

        if let Some(inode_id) = orphan_inode_id {
            self.spawn_orphan_cleanup(inode_id);
        }

        usize::try_from(staging_inode.size)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("staging size exceeds usize")))
    }

    async fn abort_staged_write(&self, staging_inode_id: u64) -> Result<()> {
        self.cleanup_staging_inode(staging_inode_id).await
    }

    async fn cleanup_marked_orphans(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let orphan_inode_ids = list_orphan_inodes(&mut txn).await?;
        let _ = txn.rollback().await;

        for inode_id in orphan_inode_ids {
            self.cleanup_orphan_inode(inode_id).await?;
        }
        Ok(())
    }

    async fn cleanup_stale_staging_writes(&self) -> Result<()> {
        let cutoff = current_unix_timestamp().saturating_sub(STALE_WRITE_STREAM_SECS);
        let mut txn = self.begin().await?;
        let staging_inode_ids = list_stale_staging_writes(&mut txn, cutoff).await?;
        let _ = txn.rollback().await;

        for inode_id in staging_inode_ids {
            self.cleanup_staging_inode(inode_id).await?;
        }
        Ok(())
    }

    async fn cleanup_staging_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin().await?;
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
                delete_pages(&mut txn, inode_id).await?;
            }
            delete_inode(&mut txn, inode_id).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    async fn cleanup_orphan_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        clear_orphan_inode(&mut txn, inode_id).await?;

        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
            if !inode.is_directory() {
                delete_pages(&mut txn, inode_id).await?;
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
}

async fn prepare_replace_file_txn(txn: &mut Transaction, path: &str) -> Result<(u64, Inode)> {
    ensure_parents(txn, path).await?;
    let (parent_inode, name) = resolve_parent(txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let mut inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        delete_pages(txn, existing_inode_id).await?;
        inode.size = 0;
        inode.page_count = 0;
        Ok((existing_inode_id, inode))
    } else {
        let inode_id = alloc_inode(txn).await?;
        let inode = Inode::new_file(inode_id, 0o644);
        link(txn, parent_inode, &name, inode_id).await?;
        Ok((inode_id, inode))
    }
}

async fn prepare_write_at_file_txn(txn: &mut Transaction, path: &str) -> Result<(u64, Inode)> {
    ensure_parents(txn, path).await?;
    let (parent_inode, name) = resolve_parent(txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        Ok((existing_inode_id, inode))
    } else {
        let inode_id = alloc_inode(txn).await?;
        let inode = Inode::new_file(inode_id, 0o644);
        save_inode(txn, &inode).await?;
        link(txn, parent_inode, &name, inode_id).await?;
        Ok((inode_id, inode))
    }
}

fn scan_end_key(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    for i in (0..end.len()).rev() {
        if end[i] != u8::MAX {
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    vec![0xFF]
}

async fn load_superblock(txn: &mut Transaction) -> Result<Superblock> {
    let data = txn
        .get(keys::superblock_key())
        .await?
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("superblock not found")))?;
    let sb: Superblock = serde_json::from_slice(&data)?;
    Ok(sb)
}

async fn save_superblock(txn: &mut Transaction, sb: &Superblock) -> Result<()> {
    let data = serde_json::to_vec(sb)?;
    txn.put(keys::superblock_key(), data).await?;
    Ok(())
}

async fn alloc_inode(txn: &mut Transaction) -> Result<u64> {
    let mut sb = load_superblock(txn).await?;
    let id = sb.next_inode;
    sb.next_inode += 1;
    save_superblock(txn, &sb).await?;
    Ok(id)
}

async fn load_inode(txn: &mut Transaction, inode_id: u64) -> Result<Option<Inode>> {
    match txn.get(keys::inode_key(inode_id)).await? {
        Some(data) => {
            let inode: Inode = serde_json::from_slice(&data)?;
            Ok(Some(inode))
        }
        None => Ok(None),
    }
}

async fn save_inode(txn: &mut Transaction, inode: &Inode) -> Result<()> {
    let data = serde_json::to_vec(inode)?;
    txn.put(keys::inode_key(inode.id), data).await?;
    Ok(())
}

async fn delete_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::inode_key(inode_id)).await?;
    Ok(())
}

async fn lookup(txn: &mut Transaction, parent_inode: u64, name: &str) -> Result<Option<u64>> {
    match txn.get(keys::dir_entry_key(parent_inode, name)).await? {
        Some(data) if data.len() == 8 => {
            let id = u64::from_be_bytes(data.as_slice().try_into()?);
            Ok(Some(id))
        }
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

async fn link(
    txn: &mut Transaction,
    parent_inode: u64,
    name: &str,
    child_inode: u64,
) -> Result<()> {
    txn.put(
        keys::dir_entry_key(parent_inode, name),
        child_inode.to_be_bytes().to_vec(),
    )
    .await?;
    Ok(())
}

async fn unlink(txn: &mut Transaction, parent_inode: u64, name: &str) -> Result<()> {
    txn.delete(keys::dir_entry_key(parent_inode, name)).await?;
    Ok(())
}

async fn list_dir(txn: &mut Transaction, parent_inode: u64) -> Result<Vec<(String, u64)>> {
    let prefix = keys::dir_prefix(parent_inode);
    let end = scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut out = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let value = pair.1;
        if key.len() <= prefix.len() || value.len() != 8 {
            continue;
        }
        let name = match String::from_utf8(key[prefix.len()..].to_vec()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let child_inode = u64::from_be_bytes(value.as_slice().try_into()?);
        out.push((name, child_inode));
    }
    Ok(out)
}

async fn read_page(txn: &mut Transaction, inode_id: u64, page_num: u64) -> Result<Option<Vec<u8>>> {
    let data = txn.get(keys::page_key(inode_id, page_num)).await?;
    Ok(data)
}

async fn read_file_range_from_txn(
    txn: &mut Transaction,
    inode_id: u64,
    file_size: u64,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    if offset >= file_size || length == 0 {
        return Ok(Vec::new());
    }

    let requested_len = u64::try_from(length)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("requested length exceeds u64")))?;
    let actual_len_u64 = requested_len.min(file_size - offset);
    let actual_len = usize::try_from(actual_len_u64).map_err(|_| {
        anyhow!(EmbeddedFsError::internal(
            "requested length exceeds addressable memory"
        ))
    })?;

    let file_end = offset
        .checked_add(actual_len_u64)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("read range overflow")))?;

    let mut data = Vec::with_capacity(actual_len);
    if let Some((start_page, end_page)) = page_range(offset, actual_len_u64) {
        for page_num in start_page..=end_page {
            let page_data = match read_page(txn, inode_id, page_num).await? {
                Some(mut page) => {
                    if page.len() < PAGE_SIZE {
                        page.resize(PAGE_SIZE, 0);
                    }
                    page
                }
                None => vec![0u8; PAGE_SIZE],
            };
            let (start, end) = page_byte_range(page_num, offset, file_end);
            data.extend_from_slice(&page_data[start..end]);
        }
    }

    Ok(data)
}

async fn write_file_chunk_to_txn(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &mut Inode,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }

    let write_len = u64::try_from(data.len())
        .map_err(|_| anyhow!(EmbeddedFsError::internal("write length exceeds u64")))?;
    let write_end = offset
        .checked_add(write_len)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("write range overflow")))?;

    if let Some((start_page, end_page)) = page_range(offset, write_len) {
        let mut data_offset = 0usize;
        for page_num in start_page..=end_page {
            let (page_start, page_end) = page_byte_range(page_num, offset, write_end);
            let chunk_len = page_end - page_start;
            let next_offset = data_offset + chunk_len;
            let chunk = &data[data_offset..next_offset];

            let is_partial = page_start != 0 || page_end != PAGE_SIZE;
            if is_partial {
                let mut page_data = match read_page(txn, inode_id, page_num).await? {
                    Some(mut page) => {
                        if page.len() < PAGE_SIZE {
                            page.resize(PAGE_SIZE, 0);
                        }
                        page
                    }
                    None => vec![0u8; PAGE_SIZE],
                };
                page_data[page_start..page_end].copy_from_slice(chunk);
                write_page(txn, inode_id, page_num, &page_data).await?;
            } else {
                write_page(txn, inode_id, page_num, chunk).await?;
            }

            data_offset = next_offset;
        }
    }

    inode.size = inode.size.max(write_end);
    inode.page_count = pages_needed(inode.size);
    Ok(())
}

async fn mark_staging_write(txn: &mut Transaction, inode_id: u64, updated_at: i64) -> Result<()> {
    txn.put(
        keys::staging_write_key(inode_id),
        updated_at.to_be_bytes().to_vec(),
    )
    .await?;
    Ok(())
}

async fn clear_staging_write(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::staging_write_key(inode_id)).await?;
    Ok(())
}

async fn mark_orphan_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.put(keys::orphan_inode_key(inode_id), Vec::new())
        .await?;
    Ok(())
}

async fn clear_orphan_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::orphan_inode_key(inode_id)).await?;
    Ok(())
}

async fn list_orphan_inodes(txn: &mut Transaction) -> Result<Vec<u64>> {
    let prefix = keys::orphan_inode_prefix();
    let end = scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut inode_ids = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        if let Some(inode_id) = parse_marked_inode_id(&prefix, &key) {
            inode_ids.push(inode_id);
        }
    }
    Ok(inode_ids)
}

async fn list_stale_staging_writes(txn: &mut Transaction, cutoff: i64) -> Result<Vec<u64>> {
    let prefix = keys::staging_write_prefix();
    let end = scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut inode_ids = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let value = pair.1;
        let Some(inode_id) = parse_marked_inode_id(&prefix, &key) else {
            continue;
        };
        let Some(updated_at) = parse_staging_write_timestamp(&value) else {
            continue;
        };
        if updated_at <= cutoff {
            inode_ids.push(inode_id);
        }
    }
    Ok(inode_ids)
}

fn parse_marked_inode_id(prefix: &[u8], key: &[u8]) -> Option<u64> {
    let suffix = key.strip_prefix(prefix)?;
    let bytes: [u8; 8] = suffix.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn parse_staging_write_timestamp(value: &[u8]) -> Option<i64> {
    let bytes: [u8; 8] = value.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

fn current_unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_not_found_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<EmbeddedFsError>(),
        Some(EmbeddedFsError::NotFound(_))
    )
}

async fn write_page(
    txn: &mut Transaction,
    inode_id: u64,
    page_num: u64,
    data: &[u8],
) -> Result<()> {
    let mut page_data = data.to_vec();
    if page_data.len() < PAGE_SIZE {
        page_data.resize(PAGE_SIZE, 0);
    }
    txn.put(keys::page_key(inode_id, page_num), page_data)
        .await?;
    Ok(())
}

async fn delete_pages(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    let prefix = keys::page_prefix(inode_id);
    let end = scan_end_key(&prefix);
    let pairs = txn.scan(prefix..end, u32::MAX).await?;
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        txn.delete(key).await?;
    }
    Ok(())
}

async fn resolve_path(txn: &mut Transaction, path: &str) -> Result<(u64, Inode)> {
    let path = normalize_path(path);
    if path == "/" {
        let inode = load_inode(txn, ROOT_INODE)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("root inode missing")))?;
        return Ok((ROOT_INODE, inode));
    }

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current_inode = ROOT_INODE;

    for (i, part) in parts.iter().enumerate() {
        let child_inode = lookup(txn, current_inode, part)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;

        if i < parts.len() - 1 {
            let inode = load_inode(txn, child_inode)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;
            if !inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(part)));
            }
        }

        current_inode = child_inode;
    }

    let inode = load_inode(txn, current_inode)
        .await?
        .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;
    Ok((current_inode, inode))
}

async fn ensure_parents(txn: &mut Transaction, path: &str) -> Result<()> {
    let normalized = normalize_path(path);

    // If path is root or empty, parent is root which always exists
    if normalized == "/" {
        return Ok(());
    }

    let (parent_path, _) = normalized.rsplit_once('/').unwrap_or(("", &normalized));
    let parent_path = if parent_path.is_empty() {
        "/"
    } else {
        parent_path
    };

    // If parent is root, it always exists
    if parent_path == "/" {
        return Ok(());
    }

    // Walk the parent path components from root, creating any missing directories
    let parts: Vec<&str> = parent_path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current_inode = ROOT_INODE;

    for part in parts {
        if let Some(next_inode_id) = lookup(txn, current_inode, part).await? {
            let next_inode = load_inode(txn, next_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling dir entry")))?;
            if !next_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(part)));
            }
            current_inode = next_inode_id;
        } else {
            // Create missing directory
            let new_inode_id = alloc_inode(txn).await?;
            let inode = Inode::new_directory(new_inode_id, 0o755);
            save_inode(txn, &inode).await?;
            link(txn, current_inode, part, new_inode_id).await?;
            current_inode = new_inode_id;
        }
    }
    Ok(())
}

async fn resolve_parent(txn: &mut Transaction, path: &str) -> Result<(u64, String)> {
    let path = normalize_path(path);
    if path == "/" {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "cannot get parent of root".to_string()
        )));
    }

    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
    let parent_path = if parent.is_empty() { "/" } else { parent };

    let (parent_inode, parent_node) = resolve_path(txn, parent_path).await?;
    if !parent_node.is_directory() {
        return Err(anyhow!(EmbeddedFsError::not_directory(parent_path)));
    }

    Ok((parent_inode, name.to_string()))
}

fn normalize_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

fn pages_needed(size: u64) -> u64 {
    if size == 0 {
        0
    } else {
        size.div_ceil(PAGE_SIZE as u64)
    }
}

/// Compute the page range [start_page, end_page] for a byte range [offset, offset+length).
/// Returns None if length is 0.
#[allow(dead_code)]
fn page_range(offset: u64, length: u64) -> Option<(u64, u64)> {
    if length == 0 {
        return None;
    }
    let start = offset / PAGE_SIZE as u64;
    let end_byte = offset.saturating_add(length - 1);
    let end = end_byte / PAGE_SIZE as u64;
    Some((start, end))
}

/// Compute the byte range within a page that a [file_offset, file_offset+length) range touches.
/// Returns (start_in_page, end_in_page) where the range is [start_in_page, end_in_page).
#[allow(dead_code)]
fn page_byte_range(page_num: u64, file_offset: u64, file_end: u64) -> (usize, usize) {
    let page_start = page_num * PAGE_SIZE as u64;
    let page_end = page_start.saturating_add(PAGE_SIZE as u64);
    let start = if file_offset > page_start {
        (file_offset - page_start) as usize
    } else {
        0
    };
    let end = if file_end < page_end {
        (file_end - page_start) as usize
    } else {
        PAGE_SIZE
    };
    (start, end)
}

async fn remove_inode_recursive(txn: &mut Transaction, inode_id: u64, inode: Inode) -> Result<u64> {
    if !inode.is_directory() {
        delete_pages(txn, inode_id).await?;
        delete_inode(txn, inode_id).await?;
        return Ok(1);
    }

    let entries = list_dir(txn, inode_id).await?;
    let mut removed = 1;
    for (name, child_id) in entries {
        let child_inode = load_inode(txn, child_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling directory entry")))?;
        removed += Box::pin(remove_inode_recursive(txn, child_id, child_inode)).await?;
        unlink(txn, inode_id, &name).await?;
    }
    delete_inode(txn, inode_id).await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    // normalize_path tests
    #[test]
    fn test_normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn test_normalize_path_empty() {
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn test_normalize_path_trailing_slash() {
        assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
    }

    #[test]
    fn test_normalize_path_no_trailing_slash() {
        assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
    }

    #[test]
    fn test_normalize_path_root_trailing_slash() {
        assert_eq!(normalize_path("/"), "/");
    }

    // pages_needed tests
    #[test]
    fn test_pages_needed_zero() {
        assert_eq!(pages_needed(0), 0);
    }

    #[test]
    fn test_pages_needed_one_byte() {
        assert_eq!(pages_needed(1), 1);
    }

    #[test]
    fn test_pages_needed_exact_page() {
        assert_eq!(pages_needed(16384), 1); // PAGE_SIZE = 16 * 1024
    }

    #[test]
    fn test_pages_needed_one_over() {
        assert_eq!(pages_needed(16385), 2);
    }

    #[test]
    fn test_pages_needed_two_pages() {
        assert_eq!(pages_needed(32768), 2);
    }

    #[test]
    fn test_pages_needed_large() {
        assert_eq!(pages_needed(1_000_000), 62); // ceil(1000000 / 16384)
    }

    #[test]
    fn test_pages_needed_last_byte_of_page() {
        assert_eq!(pages_needed((PAGE_SIZE - 1) as u64), 1);
    }

    #[test]
    fn test_pages_needed_three_pages_minus_one() {
        assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) - 1), 3);
    }

    #[test]
    fn test_pages_needed_three_pages_plus_one() {
        assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) + 1), 4);
    }

    #[test]
    fn test_pages_needed_u64_max() {
        assert_eq!(pages_needed(u64::MAX), u64::MAX.div_ceil(PAGE_SIZE as u64));
    }

    #[test]
    fn test_page_range_zero_length() {
        assert_eq!(page_range(0, 0), None);
    }

    #[test]
    fn test_read_at_page_range_single_page() {
        assert_eq!(page_range(123, 456), Some((0, 0)));
    }

    #[test]
    fn test_read_at_page_range_cross_boundary() {
        assert_eq!(page_range((PAGE_SIZE - 2) as u64, 4), Some((0, 1)));
    }

    #[test]
    fn test_read_at_page_range_exact_page() {
        assert_eq!(page_range(PAGE_SIZE as u64, PAGE_SIZE as u64), Some((1, 1)));
    }

    #[test]
    fn test_write_at_page_range_partial_first() {
        assert_eq!(
            page_range((PAGE_SIZE / 2) as u64, PAGE_SIZE as u64),
            Some((0, 1))
        );
    }

    #[test]
    fn test_write_at_page_range_full_pages() {
        assert_eq!(
            page_range(PAGE_SIZE as u64, (PAGE_SIZE * 2) as u64),
            Some((1, 2))
        );
    }

    #[test]
    fn test_page_range_single_byte_page_start() {
        assert_eq!(page_range((PAGE_SIZE * 3) as u64, 1), Some((3, 3)));
    }

    #[test]
    fn test_page_range_single_byte_page_end() {
        assert_eq!(page_range((PAGE_SIZE - 1) as u64, 1), Some((0, 0)));
    }

    #[test]
    fn test_page_range_three_pages_plus_tail() {
        assert_eq!(page_range(10, (PAGE_SIZE as u64 * 3) + 5), Some((0, 3)));
    }

    #[test]
    fn test_page_range_large_offset() {
        let base = 1_000_000u64 * PAGE_SIZE as u64;
        assert_eq!(
            page_range(base + 7, (PAGE_SIZE as u64 * 2) + 1),
            Some((1_000_000, 1_000_002))
        );
    }

    #[test]
    fn test_page_range_u64_max_single_byte() {
        assert_eq!(
            page_range(u64::MAX, 1),
            Some((u64::MAX / PAGE_SIZE as u64, u64::MAX / PAGE_SIZE as u64))
        );
    }

    #[test]
    fn test_page_byte_range_single_byte_at_page_start() {
        assert_eq!(
            page_byte_range(2, (PAGE_SIZE as u64) * 2, (PAGE_SIZE as u64) * 2 + 1),
            (0, 1)
        );
    }

    #[test]
    fn test_page_byte_range_single_byte_in_middle() {
        let start = (PAGE_SIZE as u64) * 4 + 1234;
        assert_eq!(page_byte_range(4, start, start + 1), (1234, 1235));
    }

    #[test]
    fn test_page_byte_range_single_byte_at_page_end() {
        let end = (PAGE_SIZE as u64) * 5;
        assert_eq!(page_byte_range(4, end - 1, end), (PAGE_SIZE - 1, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_exact_full_page() {
        let start = PAGE_SIZE as u64;
        let end = start + PAGE_SIZE as u64;
        assert_eq!(page_byte_range(1, start, end), (0, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_first_page_of_cross_boundary() {
        let start = (PAGE_SIZE - 10) as u64;
        let end = start + 100;
        assert_eq!(page_byte_range(0, start, end), (PAGE_SIZE - 10, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_second_page_of_cross_boundary() {
        let start = (PAGE_SIZE - 10) as u64;
        let end = start + 100;
        assert_eq!(page_byte_range(1, start, end), (0, 90));
    }

    #[test]
    fn test_page_byte_range_middle_page_three_pages() {
        let start = (PAGE_SIZE as u64) * 2 + 50;
        let end = start + (PAGE_SIZE as u64 * 3) + 10;
        assert_eq!(page_byte_range(4, start, end), (0, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_last_page_three_pages() {
        let start = (PAGE_SIZE as u64) * 2 + 50;
        let end = start + (PAGE_SIZE as u64 * 3) + 10;
        assert_eq!(page_byte_range(5, start, end), (0, 60));
    }

    #[test]
    fn test_page_byte_range_large_offset() {
        let base = (PAGE_SIZE as u64) * 2_000_000;
        assert_eq!(page_byte_range(2_000_000, base + 7, base + 20), (7, 20));
    }

    #[test]
    fn test_page_byte_range_u64_max_page() {
        let last_page = u64::MAX / PAGE_SIZE as u64;
        let page_start = last_page * PAGE_SIZE as u64;
        let file_end = page_start + 17;
        assert_eq!(
            page_byte_range(last_page, page_start + 5, file_end),
            (5, 17)
        );
    }

    #[test]
    fn test_truncate_page_count() {
        assert_eq!(pages_needed(0), 0);
        assert_eq!(pages_needed(1), 1);
        assert_eq!(pages_needed((PAGE_SIZE as u64) - 1), 1);
        assert_eq!(pages_needed(PAGE_SIZE as u64), 1);
        assert_eq!(pages_needed((PAGE_SIZE as u64) + 1), 2);
    }

    // scan_end_key tests
    // rename contract: cycle detection + path normalization
    #[test]
    fn test_rename_cycle_detection_logic() {
        // Simulates the cycle guard: new_path starts with old_path + "/"
        let old = normalize_path("/a/b");
        let new = normalize_path("/a/b/c/d");
        assert!(
            new.starts_with(&format!("{old}/")),
            "moving /a/b into /a/b/c/d is a directory cycle"
        );
    }

    #[test]
    fn test_rename_no_cycle_for_sibling() {
        let old = normalize_path("/a/b");
        let new = normalize_path("/a/b2");
        assert!(
            !new.starts_with(&format!("{old}/")),
            "/a/b → /a/b2 is not a cycle (sibling, not subtree)"
        );
    }

    #[test]
    fn test_rename_no_cycle_for_parent() {
        let old = normalize_path("/a/b/c");
        let new = normalize_path("/a");
        assert!(
            !new.starts_with(&format!("{old}/")),
            "moving deeper path to shallower is not a cycle"
        );
    }

    #[test]
    fn test_rename_same_path_noop() {
        let old = normalize_path("/foo/bar/");
        let new = normalize_path("/foo/bar");
        assert_eq!(
            old, new,
            "trailing slash normalization makes paths equal → no-op"
        );
    }

    #[test]
    fn test_rename_root_normalized() {
        let path = normalize_path("/");
        assert_eq!(path, "/");
    }

    #[test]
    fn test_normalize_path_collapses_repeated_slashes() {
        assert_eq!(normalize_path("/a//b"), "/a/b");
        assert_eq!(normalize_path("/a///b/c"), "/a/b/c");
        assert_eq!(normalize_path("//a//b//"), "/a/b");
        assert_eq!(normalize_path("///"), "/");
    }

    #[test]
    fn test_cycle_guard_with_repeated_slashes() {
        // /a//b/c normalizes to /a/b/c — the cycle guard must detect
        // that moving /a/b under /a/b/c is a cycle even with repeated slashes.
        let old = normalize_path("/a/b");
        let new = normalize_path("/a//b/c");
        assert!(
            new.starts_with(&format!("{old}/")),
            "repeated slashes must not bypass cycle guard"
        );
    }

    #[test]
    fn test_scan_end_key_simple() {
        let prefix = b"_fs_D";
        let end = scan_end_key(prefix);
        assert_eq!(end, b"_fs_E"); // 'D' + 1 = 'E'
    }

    #[test]
    fn test_scan_end_key_is_exclusive_upper_bound() {
        let prefix = b"_fs_S";
        let end = scan_end_key(prefix);
        // prefix < end
        assert!(prefix.to_vec() < end);
    }

    #[test]
    fn test_scan_end_key_with_bytes() {
        let prefix = vec![0x01, 0x02, 0x03];
        let end = scan_end_key(&prefix);
        assert_eq!(end, vec![0x01, 0x02, 0x04]);
    }

    // ── Behavioral rename tests (require TiKV) ─────────────────────────
    //
    // These tests exercise actual rename() calls on EmbeddedPageFs and
    // verify filesystem state.  They are #[ignore] because they need a
    // running TiKV cluster (PD_ENDPOINTS env var).
    //
    //   cargo test -p db9-server rename_behavioral -- --ignored

    async fn make_fs() -> EmbeddedPageFs {
        let pd = std::env::var("PD_ENDPOINTS").unwrap_or("127.0.0.1:2379".into());
        let config = tikv_client::Config::default().with_default_keyspace();
        let client = TransactionClient::new_with_config(vec![pd], config)
            .await
            .expect("TiKV connection required for behavioral tests");
        let fs = EmbeddedPageFs::new(Arc::new(client));
        fs.init_filesystem().await.expect("init_filesystem");
        fs
    }

    /// Helper: ensure directory exists (idempotent).
    async fn ensure_dir(fs: &EmbeddedPageFs, path: &str) {
        let _ = fs.mkdir(path, true).await;
    }

    /// Helper: clean up a path (file or dir) — best effort.
    async fn cleanup(fs: &EmbeddedPageFs, path: &str) {
        let _ = fs.remove_recursive(path).await;
        let _ = fs.remove(path).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_basic_file() {
        let fs = make_fs().await;
        let dir = "/test_rename_basic";
        cleanup(&fs, dir).await;
        ensure_dir(&fs, dir).await;

        let old = &format!("{dir}/a.txt");
        let new = &format!("{dir}/b.txt");
        fs.write_file(old, b"hello").await.unwrap();

        fs.rename(old, new).await.unwrap();

        // old name must be gone
        assert!(fs.stat(old).await.is_err(), "old path should not exist");
        // new name must exist with same content
        let data = fs.read_file(new).await.unwrap();
        assert_eq!(data, b"hello", "content must be preserved");

        cleanup(&fs, dir).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_cross_directory() {
        let fs = make_fs().await;
        let base = "/test_rename_cross";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/src")).await;
        ensure_dir(&fs, &format!("{base}/dst")).await;

        let old = &format!("{base}/src/file.txt");
        let new = &format!("{base}/dst/file.txt");
        fs.write_file(old, b"cross").await.unwrap();

        fs.rename(old, new).await.unwrap();

        assert!(fs.stat(old).await.is_err());
        assert_eq!(fs.read_file(new).await.unwrap(), b"cross");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_read_file_stream_behavioral_matches_read_file() {
        let fs = make_fs().await;
        let base = "/test_read_file_stream";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/large.bin");
        let data: Vec<u8> = (0..(PAGE_SIZE * 6 + 123))
            .map(|idx| (idx % 251) as u8)
            .collect();
        fs.write_file(path, &data).await.unwrap();

        let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).await.unwrap();

        assert_eq!(streamed, data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_behavioral_matches_write_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/streamed.bin");
        let data: Vec<u8> = (0..(PAGE_SIZE * 5 + 77))
            .map(|idx| (idx % 239) as u8)
            .collect();

        let mut writer = fs.begin_write_stream(path).await.unwrap();
        for chunk in data.chunks(11_111) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, data.len());
        assert_eq!(fs.read_file(path).await.unwrap(), data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_abort_preserves_existing_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream_abort";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/stable.bin");
        let original = b"stable-before-abort".to_vec();
        fs.write_file(path, &original).await.unwrap();

        let mut writer = fs.begin_write_stream(path).await.unwrap();
        writer
            .write_chunk(b"new-data-that-must-not-commit")
            .await
            .unwrap();
        writer.abort().await.unwrap();

        assert_eq!(fs.read_file(path).await.unwrap(), original);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_replaces_existing_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream_replace";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/replace.bin");
        fs.write_file(path, b"old-data").await.unwrap();

        let new_data: Vec<u8> = (0..(PAGE_SIZE * 4 + 19))
            .map(|idx| (idx % 251) as u8)
            .collect();
        let mut writer = fs.begin_write_stream(path).await.unwrap();
        for chunk in new_data.chunks(8192) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, new_data.len());
        assert_eq!(fs.read_file(path).await.unwrap(), new_data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_directory() {
        let fs = make_fs().await;
        let base = "/test_rename_dir";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/old_dir")).await;
        fs.write_file(&format!("{base}/old_dir/child.txt"), b"nested")
            .await
            .unwrap();

        fs.rename(&format!("{base}/old_dir"), &format!("{base}/new_dir"))
            .await
            .unwrap();

        assert!(fs.stat(&format!("{base}/old_dir")).await.is_err());
        let children = fs.readdir(&format!("{base}/new_dir")).await.unwrap();
        assert!(
            children.iter().any(|(name, _)| name == "child.txt"),
            "children must follow renamed directory"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_source_enoent() {
        let fs = make_fs().await;
        let err = fs
            .rename("/nonexistent_path_xyz", "/somewhere")
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "missing source should produce ENOENT-equivalent, got: {fs_err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_dest_parent_enoent_before_einval() {
        let fs = make_fs().await;
        let base = "/test_rename_parent_precedence";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/a")).await;

        let err = fs
            .rename(&format!("{base}/a"), &format!("{base}/a/missing/x"))
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "missing destination parent should produce ENOENT-equivalent before cycle EINVAL, got: {fs_err}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_cycle_einval() {
        let fs = make_fs().await;
        let base = "/test_rename_cycle";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/a/b/c")).await;

        let err = fs
            .rename(&format!("{base}/a"), &format!("{base}//a/b/c/moved"))
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::InvalidInput(_)),
            "cycle should produce EINVAL-equivalent, got: {fs_err}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_root_rejected() {
        let fs = make_fs().await;
        let err = fs.rename("/", "/newroot").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot rename root"),
            "root rename must be rejected: {msg}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_same_path_missing_source_enoent() {
        let fs = make_fs().await;
        let err = fs
            .rename("/nonexistent_same", "/nonexistent_same")
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "rename(missing, missing) with identical paths must return ENOENT, got: {fs_err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_same_path_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_noop";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        let path = &format!("{base}/f.txt");
        fs.write_file(path, b"stable").await.unwrap();

        // trailing slash on destination requires a directory target
        let err = fs.rename(path, &format!("{path}/")).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        let data = fs.read_file(path).await.unwrap();
        assert_eq!(data, b"stable", "failed rename must not corrupt data");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_dest_leaf_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_missing_leaf_trailing_slash";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        let src = &format!("{base}/f.txt");
        let dst = &format!("{base}/missing/");
        fs.write_file(src, b"stable").await.unwrap();

        let err = fs.rename(src, dst).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with missing leaf should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        assert_eq!(
            fs.read_file(src).await.unwrap(),
            b"stable",
            "failed rename must not move source file"
        );
        assert!(fs.stat(&format!("{base}/missing")).await.is_err());

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_existing_dir_dest_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_existing_dir_trailing_slash";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/existing_dir")).await;
        let src = &format!("{base}/f.txt");
        let dst = &format!("{base}/existing_dir/");
        fs.write_file(src, b"stable").await.unwrap();

        let err = fs.rename(src, dst).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with existing directory destination should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        assert_eq!(
            fs.read_file(src).await.unwrap(),
            b"stable",
            "failed rename must not move source file"
        );

        cleanup(&fs, base).await;
    }

    /// Regression test for P0 data-loss bug (#1680):
    /// If publish_staged_write commits at TiKV Raft level but the client
    /// observes a timeout, finish() calls abort_staged_write on the
    /// now-published inode. Without the nlink guard, this deletes the
    /// live file's pages and inode.
    #[tokio::test]
    #[ignore]
    async fn test_cleanup_staging_inode_skips_published_file() {
        let fs = make_fs().await;
        let base = "/test_nlink_guard";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/published.bin");
        let data = b"important-data-must-survive";

        // 1. Allocate a staging inode (nlink=0) and write data into it.
        let mut txn = fs.begin().await.unwrap();
        let inode_id = alloc_inode(&mut txn).await.unwrap();
        let mut inode = Inode::new_file(inode_id, 0o644);
        inode.nlink = 0;
        save_inode(&mut txn, &inode).await.unwrap();
        mark_staging_write(&mut txn, inode_id, current_unix_timestamp())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        fs.flush_staged_write_chunk(inode_id, 0, data)
            .await
            .unwrap();

        // 2. Publish the staging inode to the target path (sets nlink=1).
        let written = fs.publish_staged_write(path, inode_id).await.unwrap();
        assert_eq!(written, data.len());

        // 3. Simulate abort-after-ambiguous-commit: call cleanup_staging_inode
        //    on the now-published inode. The nlink guard must prevent deletion.
        fs.cleanup_staging_inode(inode_id).await.unwrap();

        // 4. The published file must still be fully readable.
        let readback = fs.read_file(path).await.unwrap();
        assert_eq!(
            readback, data,
            "cleanup_staging_inode must not delete a published file (nlink > 0)"
        );

        cleanup(&fs, base).await;
    }
}
