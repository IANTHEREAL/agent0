use anyhow::{anyhow, Result};
use tracing::warn;

use super::backend::FsBackend;

/// Check if a path contains glob metacharacters.
pub(crate) fn is_glob_pattern(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('[')
}

/// Infer the maximum directory walk depth from a glob pattern.
///
/// - `./*.rs`        → 0 (only scan prefix directory)
/// - `./src/*/*.rs`  → 1 (one level of subdirectories)
/// - `./**/*.rs`     → 20 (effectively unlimited)
fn infer_glob_max_depth(pattern: &str) -> usize {
    let prefix = glob_prefix_dir(pattern);
    let suffix = pattern[prefix.len()..].trim_start_matches('/');

    if suffix.contains("**") {
        return 20; // effectively unlimited
    }

    // Count directory separators in the glob suffix.
    // "./*.rs"       → suffix="*.rs"   → 0 separators → depth 0
    // "./src/*/*.rs" → suffix="*/*.rs"  → 1 separator  → depth 1
    suffix.matches('/').count()
}

/// Expand a glob pattern into matching file paths.
/// Returns sorted list of matching file paths (directories excluded).
pub(crate) async fn expand_glob(
    backend: &dyn FsBackend,
    pattern: &str,
    max_files: usize,
    exclude_pattern: Option<&str>,
) -> Result<Vec<String>> {
    let matcher = globset::Glob::new(pattern)
        .map_err(|err| anyhow!("fs9: invalid glob pattern '{pattern}': {err}"))?
        .compile_matcher();
    let exclude_set = build_exclude_globset(exclude_pattern)?;
    let prefix = glob_prefix_dir(pattern);
    let include_dotfiles = pattern.starts_with('.') || pattern.contains("/.");
    let max_depth = infer_glob_max_depth(pattern);

    let mut results = Vec::new();
    walk_dir(
        backend,
        prefix,
        &matcher,
        &mut results,
        max_files,
        0,
        max_depth,
        include_dotfiles,
        exclude_set.as_ref(),
    )
    .await?;

    results.sort();
    Ok(results)
}

/// Find the first file matching a glob pattern (for fast schema detection).
/// Does not sort — returns as soon as one match is found.
pub(crate) async fn find_first_match(
    backend: &dyn FsBackend,
    pattern: &str,
    exclude_pattern: Option<&str>,
) -> Result<Option<String>> {
    let matcher = globset::Glob::new(pattern)
        .map_err(|err| anyhow!("fs9: invalid glob pattern '{pattern}': {err}"))?
        .compile_matcher();
    let exclude_set = build_exclude_globset(exclude_pattern)?;
    let prefix = glob_prefix_dir(pattern);
    let include_dotfiles = pattern.starts_with('.') || pattern.contains("/.");
    let max_depth = infer_glob_max_depth(pattern);

    let mut results = Vec::new();
    walk_dir(
        backend,
        prefix,
        &matcher,
        &mut results,
        1,
        0,
        max_depth,
        include_dotfiles,
        exclude_set.as_ref(),
    )
    .await?;

    Ok(results.into_iter().next())
}

async fn walk_dir(
    backend: &dyn FsBackend,
    dir: &str,
    matcher: &globset::GlobMatcher,
    results: &mut Vec<String>,
    max_files: usize,
    depth: usize,
    max_depth: usize,
    include_dotfiles: bool,
    exclude_set: Option<&globset::GlobSet>,
) -> Result<()> {
    if depth > max_depth {
        return Ok(());
    }

    let mut stack = vec![(dir.to_string(), depth)];
    while let Some((current_dir, current_depth)) = stack.pop() {
        if current_depth > max_depth {
            continue;
        }

        let entries = match backend.readdir(&current_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("fs9: skipping directory '{}': {}", current_dir, e);
                continue;
            }
        };

        for entry in entries {
            if exclude_set.is_some_and(|set| path_matches_exclude(&entry.path, set)) {
                continue;
            }

            let filename = entry.path.rsplit('/').next().unwrap_or("");
            if filename.starts_with('.') && !include_dotfiles {
                continue;
            }

            if entry.is_dir {
                if current_depth < max_depth {
                    stack.push((entry.path, current_depth + 1));
                }
                continue;
            }

            if matcher.is_match(&entry.path)
                || entry
                    .path
                    .strip_prefix("./")
                    .is_some_and(|p| matcher.is_match(p))
            {
                results.push(entry.path);
                if results.len() >= max_files {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

fn glob_prefix_dir(pattern: &str) -> &str {
    let first_meta = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    let prefix = &pattern[..first_meta];
    match prefix.rfind('/') {
        Some(pos) => &pattern[..=pos],
        None => "./",
    }
}

pub(crate) fn build_exclude_globset(
    exclude_pattern: Option<&str>,
) -> Result<Option<globset::GlobSet>> {
    let Some(exclude_pattern) = exclude_pattern else {
        return Ok(None);
    };

    let mut builder = globset::GlobSetBuilder::new();
    let mut has_patterns = false;

    for raw in exclude_pattern.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let glob = globset::Glob::new(trimmed)
            .map_err(|err| anyhow!("fs9: invalid exclude pattern '{trimmed}': {err}"))?;
        builder.add(glob);
        has_patterns = true;
    }

    if !has_patterns {
        return Ok(None);
    }

    let set = builder
        .build()
        .map_err(|err| anyhow!("fs9: invalid exclude pattern: {err}"))?;
    Ok(Some(set))
}

pub(crate) fn path_matches_exclude(path: &str, exclude_set: &globset::GlobSet) -> bool {
    exclude_set.is_match(path)
        || path
            .strip_prefix("./")
            .is_some_and(|p| exclude_set.is_match(p))
        || path
            .rsplit('/')
            .next()
            .is_some_and(|name| exclude_set.is_match(name))
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::AsyncBufRead;

    use crate::extensions::fs::backend::{FsBackend, FsFileInfo};

    use super::*;

    struct TestLocalBackend;

    fn to_file_info(path: &str, metadata: std::fs::Metadata) -> Result<FsFileInfo> {
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();
        let mtime = metadata
            .modified()
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .as_secs();

        Ok(FsFileInfo {
            path: path.to_string(),
            is_dir,
            is_file,
            is_symlink: false,
            size: metadata.len(),
            mode: if is_dir { 0o755 } else { 0o644 },
            mtime,
        })
    }

    #[async_trait]
    impl FsBackend for TestLocalBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            let metadata = std::fs::metadata(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
            to_file_info(path, metadata)
        }

        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            let metadata = std::fs::metadata(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
            if !metadata.is_dir() {
                return Err(anyhow!("fs9: not a directory: {path}"));
            }

            let mut out = Vec::new();
            for entry in std::fs::read_dir(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            {
                let entry = entry.map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
                let entry_path = entry.path().to_string_lossy().to_string();
                let entry_meta = std::fs::metadata(entry.path())
                    .map_err(|err| anyhow!("fs9: cannot stat '{entry_path}': {err}"))?;
                let mut info = to_file_info(&entry_path, entry_meta)?;
                info.is_symlink = Path::new(&entry_path).is_symlink();
                out.push(info);
            }

            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        }

        async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/db9-fs9-glob-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn test_infer_glob_max_depth() {
        assert_eq!(infer_glob_max_depth("./*.rs"), 0);
        assert_eq!(infer_glob_max_depth("/tmp/data/*.csv"), 0);
        assert_eq!(infer_glob_max_depth("*.csv"), 0);
        assert_eq!(infer_glob_max_depth("./src/*/*.rs"), 1);
        assert_eq!(infer_glob_max_depth("/a/b/*/*/*"), 2);
        assert_eq!(infer_glob_max_depth("./**/*.rs"), 20);
        assert_eq!(infer_glob_max_depth("/tmp/**/*.jsonl"), 20);
    }

    #[tokio::test]
    async fn test_find_first_match() {
        let dir = unique_base("find-first");
        fs::write(dir.join("a.csv"), b"h\n1").expect("write a.csv");
        fs::write(dir.join("b.csv"), b"h\n2").expect("write b.csv");
        fs::write(dir.join("c.txt"), b"text").expect("write c.txt");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.csv", dir.display());
        let first = find_first_match(&backend, &pattern, None)
            .await
            .expect("find_first_match should succeed");
        assert!(first.is_some());
        assert!(first.unwrap().ends_with(".csv"));

        let pattern2 = format!("{}/*.nonexistent", dir.display());
        let none = find_first_match(&backend, &pattern2, None)
            .await
            .expect("find_first_match should succeed for no matches");
        assert!(none.is_none());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_depth_limits_recursion() {
        let dir = unique_base("depth-limit");
        fs::write(dir.join("top.csv"), b"h\n1").expect("write top.csv");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("create subdir");
        fs::write(sub.join("nested.csv"), b"h\n2").expect("write nested.csv");

        let backend = TestLocalBackend;

        let shallow = format!("{}/*.csv", dir.display());
        let files = expand_glob(&backend, &shallow, 100, None)
            .await
            .expect("shallow glob");
        assert_eq!(
            files.len(),
            1,
            "shallow glob should not recurse into subdirs"
        );
        assert!(files[0].ends_with("top.csv"));

        let deep = format!("{}/**/*.csv", dir.display());
        let files = expand_glob(&backend, &deep, 100, None)
            .await
            .expect("deep glob");
        assert_eq!(files.len(), 2, "deep glob should find files in subdirs");

        cleanup(&dir);
    }

    #[test]
    fn test_is_glob_pattern() {
        assert!(is_glob_pattern("/tmp/*.csv"));
        assert!(is_glob_pattern("/tmp/**/*.jsonl"));
        assert!(is_glob_pattern("/tmp/data[0-9].csv"));
        assert!(is_glob_pattern("/tmp/file?.txt"));
        assert!(!is_glob_pattern("/tmp/data.csv"));
        assert!(!is_glob_pattern("/tmp/dir/"));
    }

    #[test]
    fn test_glob_prefix_dir() {
        assert_eq!(glob_prefix_dir("/tmp/data/*.csv"), "/tmp/data/");
        assert_eq!(glob_prefix_dir("/tmp/**/*.jsonl"), "/tmp/");
        assert_eq!(glob_prefix_dir("*.csv"), "./");
    }

    #[tokio::test]
    async fn test_expand_glob_basic() {
        let dir = unique_base("glob-basic");
        fs::write(dir.join("a.csv"), b"h\n1").expect("write a.csv");
        fs::write(dir.join("b.csv"), b"h\n2").expect("write b.csv");
        fs::write(dir.join("c.txt"), b"text").expect("write c.txt");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.csv", dir.display());
        let files = expand_glob(&backend, &pattern, 100, None)
            .await
            .expect("expand glob should succeed");
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("a.csv"));
        assert!(files[1].ends_with("b.csv"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_recursive() {
        let dir = unique_base("glob-recursive");
        fs::write(dir.join("top.csv"), b"h\n1").expect("write top.csv");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("create subdir");
        fs::write(sub.join("nested.csv"), b"h\n2").expect("write nested.csv");

        let backend = TestLocalBackend;
        let pattern = format!("{}/**/*.csv", dir.display());
        let files = expand_glob(&backend, &pattern, 100, None)
            .await
            .expect("expand recursive glob should succeed");
        assert_eq!(files.len(), 2);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_empty() {
        let dir = unique_base("glob-empty");
        let backend = TestLocalBackend;
        let pattern = format!("{}/*.nonexistent", dir.display());
        let files = expand_glob(&backend, &pattern, 100, None)
            .await
            .expect("expand empty glob should succeed");
        assert_eq!(files.len(), 0);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_max_files() {
        let dir = unique_base("glob-max");
        for i in 0..5 {
            fs::write(dir.join(format!("f{i}.csv")), b"h\n1").expect("write csv file");
        }

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.csv", dir.display());
        let files = expand_glob(&backend, &pattern, 3, None)
            .await
            .expect("glob should truncate, not error");
        assert_eq!(files.len(), 3);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_skips_dotfiles() {
        let dir = unique_base("glob-dotfiles");
        fs::write(dir.join("visible.csv"), b"h\n1").expect("write visible.csv");
        fs::write(dir.join(".hidden.csv"), b"h\n2").expect("write hidden.csv");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.csv", dir.display());
        let files = expand_glob(&backend, &pattern, 100, None)
            .await
            .expect("expand glob should succeed");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.csv"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_with_exclude() {
        let dir = unique_base("glob-exclude-basic");
        fs::write(dir.join("a.txt"), b"a").expect("write a.txt");
        fs::write(dir.join("b.csv"), b"b").expect("write b.csv");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.*", dir.display());
        let files = expand_glob(&backend, &pattern, 100, Some("*.txt"))
            .await
            .expect("expand glob with exclude should succeed");

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("b.csv"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_exclude_no_match() {
        let dir = unique_base("glob-exclude-no-match");
        fs::write(dir.join("a.txt"), b"a").expect("write a.txt");
        fs::write(dir.join("b.csv"), b"b").expect("write b.csv");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.*", dir.display());
        let files = expand_glob(&backend, &pattern, 100, Some("*.json"))
            .await
            .expect("expand glob with non-matching exclude should succeed");

        assert_eq!(files.len(), 2);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_expand_glob_exclude_all() {
        let dir = unique_base("glob-exclude-all");
        fs::write(dir.join("a.txt"), b"a").expect("write a.txt");
        fs::write(dir.join("b.csv"), b"b").expect("write b.csv");

        let backend = TestLocalBackend;
        let pattern = format!("{}/*.*", dir.display());
        let files = expand_glob(&backend, &pattern, 100, Some("*.*"))
            .await
            .expect("expand glob with full exclude should succeed");

        assert_eq!(files.len(), 0);

        cleanup(&dir);
    }
}
