use super::*;

pub(crate) async fn list_directory_entries(
    backend: &dyn backend::FsBackend,
    path: &str,
    recursive: bool,
    exclude_set: Option<Arc<globset::GlobSet>>,
) -> Result<Vec<backend::FsFileInfo>> {
    if !recursive {
        let mut entries = backend.readdir(path).await?;
        if let Some(exclude_set) = exclude_set.as_deref() {
            entries.retain(|entry| !glob::path_matches_exclude(&entry.path, exclude_set));
        }
        return Ok(entries);
    }

    let config = config::fs9_config();
    let result = backend
        .readdir_recursive(
            path,
            backend::FsRecursiveReaddirOptions {
                max_depth: config.readdir_recursive_max_depth,
                max_entries: config.readdir_recursive_max_entries,
                exclude_set,
            },
        )
        .await?;

    if result.truncated {
        warn!(
            "fs9: directory listing capped at {} entries",
            config.readdir_recursive_max_entries
        );
    }

    Ok(result.entries)
}
