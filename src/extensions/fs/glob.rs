use anyhow::{anyhow, Result};

use super::backend::FsBackend;

/// Check if a path contains glob metacharacters.
#[allow(dead_code)]
pub(crate) fn is_glob_pattern(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('[')
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

    let mut results = Vec::new();
    walk_dir(
        backend,
        prefix,
        &matcher,
        &mut results,
        max_files,
        0,
        10,
        include_dotfiles,
        exclude_set.as_ref(),
    )
    .await?;

    results.sort();
    Ok(results)
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
            Err(_) => continue,
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
    let first_meta = pattern
        .find(|c: char| c == '*' || c == '?' || c == '[')
        .unwrap_or(pattern.len());
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::extensions::fs::backend::LocalFsBackend;

    use super::*;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/pgtikv-fs9-glob-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
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

        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
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
        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
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

        let backend = LocalFsBackend::new();
        let pattern = format!("{}/*.*", dir.display());
        let files = expand_glob(&backend, &pattern, 100, Some("*.*"))
            .await
            .expect("expand glob with full exclude should succeed");

        assert_eq!(files.len(), 0);

        cleanup(&dir);
    }
}
