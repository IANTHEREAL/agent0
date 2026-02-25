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

        let file_len = usize::try_from(inode.size)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("file size exceeds addressable memory")))?;
        if data.len() > file_len {
            data.truncate(file_len);
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

async fn link(txn: &mut Transaction, parent_inode: u64, name: &str, child_inode: u64) -> Result<()> {
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

async fn write_page(txn: &mut Transaction, inode_id: u64, page_num: u64, data: &[u8]) -> Result<()> {
    let mut page_data = data.to_vec();
    if page_data.len() < PAGE_SIZE {
        page_data.resize(PAGE_SIZE, 0);
    }
    txn.put(keys::page_key(inode_id, page_num), page_data).await?;
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
    let parent_path = if parent_path.is_empty() { "/" } else { parent_path };
    
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
        assert_eq!(pages_needed(16384), 1);  // PAGE_SIZE = 16 * 1024
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
        assert_eq!(pages_needed(1_000_000), 62);  // ceil(1000000 / 16384)
    }

    // scan_end_key tests
    #[test]
    fn test_scan_end_key_simple() {
        let prefix = b"_fs_D";
        let end = scan_end_key(prefix);
        assert_eq!(end, b"_fs_E");  // 'D' + 1 = 'E'
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