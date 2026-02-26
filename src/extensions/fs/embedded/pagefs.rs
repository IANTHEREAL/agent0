use crate::extensions::fs::embedded::keys;
use crate::extensions::fs::embedded::types::*;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tikv_client::{CheckLevel, Transaction, TransactionClient, TransactionOptions};

pub(crate) struct EmbeddedPageFs {
    client: Arc<TransactionClient>,
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
            return Ok(());
        }

        if load_inode(&mut txn, ROOT_INODE).await?.is_none() {
            let root = Inode::new_directory(ROOT_INODE, 0o755);
            save_inode(&mut txn, &root).await?;
            txn.commit().await?;
            return Ok(());
        }

        let _ = txn.rollback().await;
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

        let mut data = Vec::new();
        for page_num in 0..inode.page_count {
            if let Some(page_data) = read_page(&mut txn, inode_id, page_num).await? {
                data.extend_from_slice(&page_data);
            } else {
                data.extend(std::iter::repeat_n(0u8, PAGE_SIZE));
            }
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        if data.len() > file_len {
            data.truncate(file_len);
        }

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

        let requested_len = u64::try_from(length)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("requested length exceeds u64")))?;
        let actual_len_u64 = requested_len.min(inode.size - offset);
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
                let page_data = match read_page(&mut txn, inode_id, page_num).await? {
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

        inode.touch_atime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
        Ok(data)
    }

    pub(crate) async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        let mut txn = self.begin().await?;

        // Auto-create parent directories (matching FsBackend trait contract)
        ensure_parents(&mut txn, path).await?;

        let (parent_inode, name) = resolve_parent(&mut txn, path).await?;

        let inode_id;
        let mut inode;

        if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
            let existing_inode = load_inode(&mut txn, existing_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

            if existing_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::is_directory(path)));
            }

            delete_pages(&mut txn, existing_inode_id).await?;
            inode_id = existing_inode_id;
            inode = existing_inode;
        } else {
            let new_inode_id = alloc_inode(&mut txn).await?;
            inode_id = new_inode_id;
            inode = Inode::new_file(new_inode_id, 0o644);
            link(&mut txn, parent_inode, &name, new_inode_id).await?;
        }

        for (page_num, chunk) in data.chunks(PAGE_SIZE).enumerate() {
            write_page(&mut txn, inode_id, page_num as u64, chunk).await?;
        }

        inode.size = data.len() as u64;
        inode.page_count = pages_needed(inode.size);
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;

        txn.commit().await?;
        Ok(data.len())
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

        ensure_parents(&mut txn, path).await?;

        let (parent_inode, name) = resolve_parent(&mut txn, path).await?;

        let inode_id;
        let mut inode;

        if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
            let existing_inode = load_inode(&mut txn, existing_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

            if existing_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::is_directory(path)));
            }

            inode_id = existing_inode_id;
            inode = existing_inode;
        } else {
            let new_inode_id = alloc_inode(&mut txn).await?;
            inode_id = new_inode_id;
            inode = Inode::new_file(new_inode_id, 0o644);
            save_inode(&mut txn, &inode).await?;
            link(&mut txn, parent_inode, &name, new_inode_id).await?;
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
                    let mut page_data = match read_page(&mut txn, inode_id, page_num).await? {
                        Some(mut page) => {
                            if page.len() < PAGE_SIZE {
                                page.resize(PAGE_SIZE, 0);
                            }
                            page
                        }
                        None => vec![0u8; PAGE_SIZE],
                    };
                    page_data[page_start..page_end].copy_from_slice(chunk);
                    write_page(&mut txn, inode_id, page_num, &page_data).await?;
                } else {
                    write_page(&mut txn, inode_id, page_num, chunk).await?;
                }

                data_offset = next_offset;
            }
        }

        inode.size = inode.size.max(write_end);
        inode.page_count = pages_needed(inode.size);
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

        if old_normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot rename root".to_string(),
            )));
        }

        // No-op if paths are identical
        if old_normalized == new_normalized {
            return Ok(());
        }

        // Prevent directory cycle: renaming a dir into its own subtree
        // would corrupt the directory tree (POSIX returns EINVAL for this).
        // Safe to use string prefix here because both paths are already
        // normalized (no trailing slash) and validate_path rejected "..".
        if new_normalized.starts_with(&format!("{old_normalized}/")) {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(format!(
                "cannot rename {old_normalized} into its own subdirectory {new_normalized}"
            ),)));
        }

        let mut txn = self.begin().await?;

        // Resolve the source
        let (old_inode_id, old_inode) = resolve_path(&mut txn, &old_normalized).await?;
        let (old_parent_inode, old_name) = resolve_parent(&mut txn, &old_normalized).await?;

        // Ensure parent directories of destination exist
        ensure_parents(&mut txn, &new_normalized).await?;
        let (new_parent_inode, new_name) = resolve_parent(&mut txn, &new_normalized).await?;

        // Check if destination already exists
        if let Some(existing_inode_id) = lookup(&mut txn, new_parent_inode, &new_name).await? {
            let existing_inode = load_inode(&mut txn, existing_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&new_normalized)))?;

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
    let path = if path.is_empty() { "/" } else { path };
    if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    }
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
}
