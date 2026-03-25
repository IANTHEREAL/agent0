//! Micro-benchmarks comparing `batch_write` (per-file txn) vs `batch_write_grouped`
//! (per-directory txn). Requires a running TiKV instance.
//!
//! Run with:
//!   PD_ENDPOINTS=127.0.0.1:2379 cargo test -p db9-server bench_grouped_write -- --ignored --nocapture

use super::*;
use crate::extensions::fs::backend::{FsBatchWriteFile, FsBatchWriteGroupedResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static BENCH_KEYSPACE_SEQ: AtomicU64 = AtomicU64::new(1000);

fn bench_keyspace() -> String {
    if let Ok(keyspace) = std::env::var("TIKV_KEYSPACE") {
        if !keyspace.trim().is_empty() {
            return keyspace;
        }
    }
    let seq = BENCH_KEYSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("fs9_bench_{}_{}", std::process::id(), seq)
}

async fn ensure_bench_keyspace(pd_addrs: &[String], keyspace: &str) {
    if std::env::var("TIKV_CA_PATH").is_ok() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("failed to build PD HTTP client for bench tests");

    let mut last_err = String::new();
    for attempt in 0..15 {
        let pd = &pd_addrs[attempt % pd_addrs.len()];
        let base = format!("http://{}/pd/api/v2/keyspaces", pd);
        let keyspace_url = format!("{}/{}", base, keyspace);

        if let Ok(resp) = client
            .post(&base)
            .json(&serde_json::json!({ "name": keyspace }))
            .send()
            .await
        {
            let status = resp.status();
            if !(status.is_success() || status.as_u16() == 409 || status.as_u16() == 500) {
                let _ = resp.text().await;
            }
        }

        match client.get(&keyspace_url).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!(
                    "verify keyspace via {pd} status={}, body={}",
                    status.as_u16(),
                    body
                );
            }
            Err(err) => {
                last_err = format!("verify keyspace via {pd} error: {err}");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    panic!(
        "unable to provision bench keyspace '{}' via {:?}: {}",
        keyspace, pd_addrs, last_err
    );
}

async fn make_bench_fs() -> EmbeddedPageFs {
    let pd_raw = std::env::var("PD_ENDPOINTS").unwrap_or("127.0.0.1:2379".into());
    let pd_addrs: Vec<String> = pd_raw.split(',').map(|s| s.trim().to_string()).collect();
    let keyspace = bench_keyspace();
    ensure_bench_keyspace(&pd_addrs, &keyspace).await;
    let mut config = tikv_client::Config::default().with_keyspace(&keyspace);
    if let (Ok(ca), Ok(cert), Ok(key)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        config = config.with_security(ca, cert, key);
    }
    let client = TransactionClient::new_with_config(pd_addrs, config)
        .await
        .expect("TiKV connection required for bench tests");
    let client = Arc::new(client);
    let superblock = EmbeddedPageFs::load_or_init_superblock(client.clone(), &keyspace)
        .await
        .expect("load_or_init_superblock");
    let fs = EmbeddedPageFs::new(client, keyspace, &superblock);
    fs.init_filesystem().await.expect("init_filesystem");
    fs
}

fn make_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

struct Scenario {
    name: &'static str,
    file_count: usize,
    file_size: usize,
    dir_count: usize,
}

struct BenchResult {
    scenario: &'static str,
    old_ms: f64,
    new_ms: f64,
    speedup: f64,
    subgroup_count: usize,
}

fn build_files(base: &str, scenario: &Scenario) -> Vec<FsBatchWriteFile> {
    let data = make_data(scenario.file_size);
    (0..scenario.file_count)
        .map(|i| {
            let dir_idx = i % scenario.dir_count;
            let dir = if scenario.dir_count == 1 {
                format!("{base}/d0")
            } else {
                format!("{base}/d{dir_idx}")
            };
            FsBatchWriteFile {
                path: format!("{dir}/f_{i:04}.dat"),
                data: data.clone(),
                mode: None,
            }
        })
        .collect()
}

async fn setup_dirs(fs: &EmbeddedPageFs, base: &str, dir_count: usize) {
    let _ = fs.remove_recursive(base).await;
    let _ = fs.remove(base).await;
    fs.mkdir(base, true, None).await.expect("mkdir base");
    for i in 0..dir_count {
        let dir = format!("{base}/d{i}");
        fs.mkdir(&dir, true, None).await.expect("mkdir subdir");
    }
}

async fn run_old_path(fs: &EmbeddedPageFs, files: Vec<FsBatchWriteFile>) -> std::time::Duration {
    let start = Instant::now();
    let entries = fs.batch_write(files).await.expect("batch_write failed");
    let elapsed = start.elapsed();
    for entry in &entries {
        assert!(
            entry.result.is_ok(),
            "batch_write entry {} failed: {:?}",
            entry.path,
            entry.result,
        );
    }
    elapsed
}

async fn run_new_path(
    fs: &EmbeddedPageFs,
    files: Vec<FsBatchWriteFile>,
) -> (std::time::Duration, FsBatchWriteGroupedResult) {
    let start = Instant::now();
    let result = fs
        .batch_write_grouped(files)
        .await
        .expect("batch_write_grouped failed");
    let elapsed = start.elapsed();
    for entry in &result.entries {
        assert!(
            entry.result.is_ok(),
            "batch_write_grouped entry {} failed: {:?}",
            entry.path,
            entry.result,
        );
    }
    (elapsed, result)
}

async fn verify_readable(fs: &EmbeddedPageFs, files: &[FsBatchWriteFile]) {
    for f in files {
        let data = fs
            .read_file_capped(&f.path, f.data.len() + 1024)
            .await
            .unwrap_or_else(|e| panic!("read_file_capped({}) failed: {e}", f.path));
        assert_eq!(
            data.len(),
            f.data.len(),
            "file {} size mismatch: got {} expected {}",
            f.path,
            data.len(),
            f.data.len(),
        );
        assert_eq!(data, f.data, "file {} content mismatch", f.path);
    }
}

fn print_table(results: &[BenchResult]) {
    println!();
    println!(
        "{:<45} {:>10} {:>10} {:>10} {:>10}",
        "Scenario", "Old (ms)", "New (ms)", "Speedup", "Subgroups"
    );
    println!("{}", "-".repeat(89));
    for r in results {
        println!(
            "{:<45} {:>10.1} {:>10.1} {:>9.2}x {:>10}",
            r.scenario, r.old_ms, r.new_ms, r.speedup, r.subgroup_count,
        );
    }
    println!("{}", "-".repeat(89));
    println!();
}

#[tokio::test]
#[ignore]
async fn bench_grouped_write() {
    let fs = make_bench_fs().await;
    let base = "/bench_grouped_write";

    let scenarios = vec![
        Scenario {
            name: "32 x 1KB, same dir",
            file_count: 32,
            file_size: 1024,
            dir_count: 1,
        },
        Scenario {
            name: "32 x 4KB, same dir",
            file_count: 32,
            file_size: 4096,
            dir_count: 1,
        },
        Scenario {
            name: "32 x 1KB, 4 dirs (8 per dir)",
            file_count: 32,
            file_size: 1024,
            dir_count: 4,
        },
        Scenario {
            name: "64 x 1KB, same dir (triggers subgroup chunking)",
            file_count: 64,
            file_size: 1024,
            dir_count: 1,
        },
    ];

    let mut results = Vec::new();

    for scenario in &scenarios {
        println!("--- Running: {} ---", scenario.name);

        // --- Old path ---
        let old_base = format!("{base}/old_{}", scenario.name.replace(' ', "_"));
        setup_dirs(&fs, &old_base, scenario.dir_count).await;
        let old_files = build_files(&old_base, scenario);
        let old_elapsed = run_old_path(&fs, old_files.clone()).await;

        // Verify old path correctness
        verify_readable(&fs, &old_files).await;

        // --- New path ---
        let new_base = format!("{base}/new_{}", scenario.name.replace(' ', "_"));
        setup_dirs(&fs, &new_base, scenario.dir_count).await;
        let new_files = build_files(&new_base, scenario);
        let (new_elapsed, grouped_result) = run_new_path(&fs, new_files.clone()).await;

        // Verify new path correctness
        verify_readable(&fs, &new_files).await;

        let old_ms = old_elapsed.as_secs_f64() * 1000.0;
        let new_ms = new_elapsed.as_secs_f64() * 1000.0;
        let speedup = if new_ms > 0.0 { old_ms / new_ms } else { 0.0 };

        results.push(BenchResult {
            scenario: scenario.name,
            old_ms,
            new_ms,
            speedup,
            subgroup_count: grouped_result.actual_subgroup_count,
        });
    }

    print_table(&results);

    // Cleanup
    let _ = fs.remove_recursive(base).await;
    let _ = fs.remove(base).await;
}

/// Benchmark to find the TiKV txn size ceiling / optimal subgroup size.
/// Tests writing N files (4KB each) in a single `batch_write_grouped` call
/// with varying subgroup sizes, measuring how latency scales.
#[tokio::test]
#[ignore]
async fn bench_txn_size_ceiling() {
    let fs = make_bench_fs().await;
    let base = "/bench_txn_ceiling";

    // Temporarily override subgroup size by testing grouped write at different file counts
    // within a single directory (so all files land in one subgroup).
    let file_counts = [8, 16, 32, 64, 128, 256, 512];
    let file_size = 4096; // 4KB per file

    println!();
    println!(
        "{:<20} {:>12} {:>12} {:>14} {:>12}",
        "Files (4KB each)", "Write Set", "Latency (ms)", "Per-file (ms)", "Throughput"
    );
    println!("{}", "-".repeat(74));

    for &count in &file_counts {
        let dir = format!("{base}/n{count}");
        let _ = fs.remove_recursive(&dir).await;
        let _ = fs.remove(&dir).await;
        fs.mkdir(&dir, true, None).await.expect("mkdir");

        let data = make_data(file_size);
        let files: Vec<FsBatchWriteFile> = (0..count)
            .map(|i| FsBatchWriteFile {
                path: format!("{dir}/f_{i:04}.dat"),
                data: data.clone(),
                mode: None,
            })
            .collect();

        // Use batch_write_grouped which will put all same-dir files into subgroups
        let start = Instant::now();
        let result = fs
            .batch_write_grouped(files.clone())
            .await
            .expect("batch_write_grouped failed");
        let elapsed = start.elapsed();

        for entry in &result.entries {
            assert!(entry.result.is_ok(), "entry {} failed", entry.path);
        }

        let ms = elapsed.as_secs_f64() * 1000.0;
        let per_file_ms = ms / count as f64;
        let write_set_kb = (count * file_size) / 1024;
        let throughput = format!("{:.1} MB/s", (write_set_kb as f64) / (ms / 1000.0) / 1024.0);

        println!(
            "{:<20} {:>10} KB {:>12.1} {:>14.3} {:>12}",
            format!("{count} files"),
            write_set_kb,
            ms,
            per_file_ms,
            throughput,
        );
    }

    println!("{}", "-".repeat(74));
    println!();

    // Cleanup
    let _ = fs.remove_recursive(base).await;
    let _ = fs.remove(base).await;
}
