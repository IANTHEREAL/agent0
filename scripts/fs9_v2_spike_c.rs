// Spike tool (not part of product code): exercises the proposed fs9 v2 lifecycle model
// against dev TiKV + S3, with crash/recovery simulations.

use anyhow::{anyhow, Context, Result};
use futures::future::BoxFuture;
use rand::{rngs::StdRng, RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::sync::Barrier;
use tokio::time::sleep;
use tikv_client::{
    BoundRange, CheckLevel, Config, Key, KvPair, Transaction, TransactionClient, TransactionOptions,
};

const PD_ENDPOINT: &str = "serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379";
const KEYSPACE: &str = "DEFAULT";
const BUCKET: &str = "dev-us-west-2-f02-db9-fs";

const BENCH_PREFIX: &str = "__bench_fs9v2_lifecycle__";

const COMMIT_MAX_RETRIES: usize = 5;
const COMMIT_BACKOFF_BASE_MS: u64 = 30;

const HEAD_MAX_RETRIES: usize = 5;
const HEAD_BACKOFF_BASE_MS: u64 = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InodeRec {
    gen: u64,
    #[serde(default)]
    obj_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Life {
    Uploading {
        expected_gen: u64,
        new_key: String,
        size: u64,
        created_ms: u64,
        expires_ms: u64,
    },
    Deleting {
        delete_key: String,
        reason: String,
        created_ms: u64,
    },
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn inode_key(run_id: u64, inode: u64) -> Key {
    format!("{BENCH_PREFIX}/I/{run_id}/{inode}")
        .into_bytes()
        .into()
}

fn life_key(run_id: u64, inode: u64) -> Key {
    format!("{BENCH_PREFIX}/L/{run_id}/{inode}")
        .into_bytes()
        .into()
}

fn prefix_range(prefix: &[u8]) -> BoundRange {
    // [prefix, prefix_end) where prefix_end is prefix with last byte + 1.
    // Sufficient for ASCII prefixes used in this spike.
    let mut end = prefix.to_vec();
    let mut i = end.len();
    while i > 0 {
        i -= 1;
        if end[i] != 0xFF {
            end[i] += 1;
            end.truncate(i + 1);
            return (Key::from(prefix.to_vec())..Key::from(end)).into();
        }
    }
    end.push(0);
    (Key::from(prefix.to_vec())..Key::from(end)).into()
}

fn encode_json<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(v).context("encode json")?)
}

fn decode_json<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(buf).context("decode json")?)
}

fn aws_output(args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new("aws")
        .args(args)
        .output()
        .with_context(|| format!("spawn aws {args:?}"))?;
    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Ok((code, stdout, stderr))
}

fn s3_put_object(bucket: &str, key: &str, file: &PathBuf) -> Result<()> {
    let (code, _stdout, stderr) = aws_output(&[
        "s3api",
        "put-object",
        "--bucket",
        bucket,
        "--key",
        key,
        "--body",
        file.to_str()
            .ok_or_else(|| anyhow!("non-utf8 path: {file:?}"))?,
    ])?;
    if code != 0 {
        return Err(anyhow!("s3 put-object failed: {stderr}"));
    }
    Ok(())
}

fn s3_head_object(bucket: &str, key: &str) -> Result<bool> {
    let (code, _stdout, stderr) =
        aws_output(&["s3api", "head-object", "--bucket", bucket, "--key", key])?;
    if code == 0 {
        return Ok(true);
    }
    if stderr.contains("Not Found")
        || stderr.contains("404")
        || stderr.contains("NoSuchKey")
        || stderr.contains("NotFound")
    {
        return Ok(false);
    }
    Err(anyhow!("s3 head-object failed: {stderr}"))
}

fn s3_delete_object(bucket: &str, key: &str) -> Result<()> {
    let (code, _stdout, stderr) =
        aws_output(&["s3api", "delete-object", "--bucket", bucket, "--key", key])?;
    if code == 0 {
        return Ok(());
    }
    if stderr.contains("Not Found")
        || stderr.contains("404")
        || stderr.contains("NoSuchKey")
        || stderr.contains("NotFound")
    {
        return Ok(());
    }
    Err(anyhow!("s3 delete-object failed: {stderr}"))
}

async fn begin(client: &TransactionClient) -> Result<Transaction> {
    let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
    Ok(client
        .begin_with_options(options)
        .await
        .map_err(|e| anyhow!(e))?)
}

async fn get_inode(txn: &mut Transaction, key: Key) -> Result<InodeRec> {
    match txn.get(key).await.map_err(|e| anyhow!(e))? {
        Some(buf) => decode_json(&buf),
        None => Ok(InodeRec { gen: 0, obj_key: None }),
    }
}

async fn get_life(txn: &mut Transaction, key: Key) -> Result<Option<Life>> {
    match txn.get(key).await.map_err(|e| anyhow!(e))? {
        Some(buf) => Ok(Some(decode_json(&buf)?)),
        None => Ok(None),
    }
}

fn is_retryable_commit_err(msg: &str) -> bool {
    // Best-effort: conflict strings vary; treat conflicts/timeouts as retryable.
    let m = msg.to_ascii_lowercase();
    m.contains("write conflict")
        || m.contains("conflict")
        || m.contains("retry")
        || m.contains("timeout")
        || m.contains("region error")
}

async fn run_txn_with_retry(
    client: &TransactionClient,
    mut f: impl for<'a> FnMut(&'a mut Transaction) -> BoxFuture<'a, Result<()>>,
) -> Result<()> {
    for attempt in 0..COMMIT_MAX_RETRIES {
        let mut txn = begin(client).await?;
        match f(&mut txn).await {
            Ok(()) => match txn.commit().await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let msg = format!("{e:?}");
                    let _ = txn.rollback().await;
                    if attempt + 1 == COMMIT_MAX_RETRIES || !is_retryable_commit_err(&msg) {
                        return Err(anyhow!("txn commit failed: {msg}"));
                    }
                    let backoff = COMMIT_BACKOFF_BASE_MS * (1u64 << attempt.min(4));
                    sleep(Duration::from_millis(backoff)).await;
                    continue;
                }
            },
            Err(e) => {
                let _ = txn.rollback().await;
                return Err(e);
            }
        }
    }
    Err(anyhow!("txn exceeded retries"))
}

async fn put_inode_atomic(client: &TransactionClient, ik: Key, rec: InodeRec) -> Result<()> {
    run_txn_with_retry(client, move |txn| {
        let ik = ik.clone();
        let rec = rec.clone();
        Box::pin(async move {
            txn.put(ik, encode_json(&rec)?)
                .await
                .map_err(|e| anyhow!(e))?;
            Ok(())
        })
    })
    .await
}

async fn put_life_atomic(client: &TransactionClient, lk: Key, life: Life) -> Result<()> {
    run_txn_with_retry(client, move |txn| {
        let lk = lk.clone();
        let life = life.clone();
        Box::pin(async move {
            txn.put(lk, encode_json(&life)?)
                .await
                .map_err(|e| anyhow!(e))?;
            Ok(())
        })
    })
    .await
}

async fn prepare_upload(
    client: &TransactionClient,
    run_id: u64,
    inode: u64,
    expected_gen: u64,
    new_key: String,
    size: u64,
    ttl_ms: u64,
) -> Result<()> {
    let ik = inode_key(run_id, inode);
    let lk = life_key(run_id, inode);
    run_txn_with_retry(client, move |txn| {
        let ik = ik.clone();
        let lk = lk.clone();
        let new_key = new_key.clone();
        Box::pin(async move {
            let inode_rec = get_inode(txn, ik.clone()).await?;
            if inode_rec.gen != expected_gen {
                return Err(anyhow!(
                    "prepare_upload: inode gen mismatch: expected {expected_gen}, got {}",
                    inode_rec.gen
                ));
            }
            if get_life(txn, lk.clone()).await?.is_some() {
                return Err(anyhow!("prepare_upload: lifecycle already exists"));
            }
            let now = now_ms();
            let life = Life::Uploading {
                expected_gen,
                new_key,
                size,
                created_ms: now,
                expires_ms: now.saturating_add(ttl_ms),
            };
            txn.put(lk, encode_json(&life)?)
                .await
                .map_err(|e| anyhow!(e))?;
            Ok(())
        })
    })
    .await
}

async fn head_with_retry(bucket: &str, key: &str) -> Result<bool> {
    for attempt in 0..HEAD_MAX_RETRIES {
        match s3_head_object(bucket, key) {
            Ok(exists) => return Ok(exists),
            Err(e) => {
                if attempt + 1 == HEAD_MAX_RETRIES {
                    return Err(e);
                }
                let backoff = HEAD_BACKOFF_BASE_MS * (1u64 << attempt.min(4));
                sleep(Duration::from_millis(backoff)).await;
            }
        }
    }
    Ok(false)
}

async fn publish_or_abort(
    client: &TransactionClient,
    run_id: u64,
    inode: u64,
    uploading: Life,
) -> Result<()> {
    let ik = inode_key(run_id, inode);
    let lk = life_key(run_id, inode);
    run_txn_with_retry(client, move |txn| {
        let ik = ik.clone();
        let lk = lk.clone();
        let uploading = uploading.clone();
        Box::pin(async move {
            // CAS: only act if lifecycle is still exactly what we observed.
            let cur = get_life(txn, lk.clone()).await?;
            if cur.as_ref() != Some(&uploading) {
                return Ok(());
            }

            let Life::Uploading {
                expected_gen,
                new_key,
                ..
            } = &uploading
            else {
                return Ok(());
            };

            let inode_rec = get_inode(txn, ik.clone()).await?;
            if inode_rec.gen != *expected_gen {
                // CAS failure: never overwrite; instead GC the uploaded new object.
                let del = Life::Deleting {
                    delete_key: new_key.clone(),
                    reason: "cas_failed".to_string(),
                    created_ms: now_ms(),
                };
                txn.put(lk, encode_json(&del)?)
                    .await
                    .map_err(|e| anyhow!(e))?;
                return Ok(());
            }

            let old = inode_rec.obj_key.clone();
            let updated = InodeRec {
                gen: inode_rec.gen + 1,
                obj_key: Some(new_key.clone()),
            };
            txn.put(ik, encode_json(&updated)?)
                .await
                .map_err(|e| anyhow!(e))?;

            match old {
                Some(old_key) => {
                    // Mark old data for GC via lifecycle.
                    let del = Life::Deleting {
                        delete_key: old_key,
                        reason: "replaced_old".to_string(),
                        created_ms: now_ms(),
                    };
                    txn.put(lk, encode_json(&del)?)
                        .await
                        .map_err(|e| anyhow!(e))?;
                }
                None => {
                    // No old data to delete; lifecycle can be cleared immediately.
                    txn.delete(lk).await.map_err(|e| anyhow!(e))?;
                }
            }
            Ok(())
        })
    })
    .await
}

async fn abort_expired(
    client: &TransactionClient,
    run_id: u64,
    inode: u64,
    uploading: Life,
) -> Result<()> {
    let lk = life_key(run_id, inode);
    run_txn_with_retry(client, move |txn| {
        let lk = lk.clone();
        let uploading = uploading.clone();
        Box::pin(async move {
            let cur = get_life(txn, lk.clone()).await?;
            if cur.as_ref() != Some(&uploading) {
                return Ok(());
            }
            let Life::Uploading { new_key, .. } = &uploading else {
                return Ok(());
            };
            let del = Life::Deleting {
                delete_key: new_key.clone(),
                reason: "expired".to_string(),
                created_ms: now_ms(),
            };
            txn.put(lk, encode_json(&del)?)
                .await
                .map_err(|e| anyhow!(e))?;
            Ok(())
        })
    })
    .await
}

async fn gc(client: &TransactionClient, run_id: u64, inode: u64, deleting: Life) -> Result<()> {
    let lk = life_key(run_id, inode);
    let Life::Deleting { delete_key, .. } = &deleting else {
        return Ok(());
    };

    // data-plane delete first (idempotent)
    s3_delete_object(BUCKET, delete_key)?;

    run_txn_with_retry(client, move |txn| {
        let lk = lk.clone();
        let deleting = deleting.clone();
        Box::pin(async move {
            let cur = get_life(txn, lk.clone()).await?;
            if cur.as_ref() != Some(&deleting) {
                return Ok(());
            }
            txn.delete(lk).await.map_err(|e| anyhow!(e))?;
            Ok(())
        })
    })
    .await
}

async fn scan_lifecycles(client: &TransactionClient, run_id: u64) -> Result<Vec<(u64, Life)>> {
    let prefix = format!("{BENCH_PREFIX}/L/{run_id}/");
    let range = prefix_range(prefix.as_bytes());

    let mut txn = begin(client).await?;
    let iter = txn.scan(range, 10_000).await.map_err(|e| anyhow!(e))?;
    let mut out = Vec::new();
    for KvPair(key, value) in iter {
        let raw: Vec<u8> = key.into();
        let k_str = String::from_utf8_lossy(&raw);
        let inode_str = k_str
            .rsplit('/')
            .next()
            .ok_or_else(|| anyhow!("bad key: {k_str}"))?;
        let inode: u64 = inode_str
            .parse()
            .with_context(|| format!("parse inode from key: {k_str}"))?;
        let life: Life = decode_json(&value)?;
        out.push((inode, life));
    }
    let _ = txn.rollback().await;
    Ok(out)
}

async fn recovery_sweep(client: &TransactionClient, run_id: u64, bucket: &str) -> Result<()> {
    let items = scan_lifecycles(client, run_id).await?;
    for (inode, life) in items {
        match &life {
            Life::Uploading {
                new_key,
                expires_ms,
                ..
            } => {
                let now = now_ms();
                if now >= *expires_ms {
                    abort_expired(client, run_id, inode, life.clone()).await?;
                } else if head_with_retry(bucket, new_key).await? {
                    publish_or_abort(client, run_id, inode, life.clone()).await?;
                }

                // If the lifecycle moved to Deleting, run GC now.
                let mut txn = begin(client).await?;
                let cur = get_life(&mut txn, life_key(run_id, inode)).await?;
                let _ = txn.rollback().await;
                if let Some(cur_life) = cur {
                    if matches!(cur_life, Life::Deleting { .. }) {
                        gc(client, run_id, inode, cur_life).await?;
                    }
                }
            }
            Life::Deleting { .. } => {
                gc(client, run_id, inode, life.clone()).await?;
            }
        }
    }
    Ok(())
}

fn write_random_file(path: &PathBuf, size: usize, rng: &mut StdRng) -> Result<()> {
    let mut buf = vec![0u8; size];
    rng.fill_bytes(&mut buf);
    std::fs::write(path, &buf).with_context(|| format!("write file {path:?}"))?;
    Ok(())
}

async fn scenario_crash_after_upload_before_publish(
    client: &TransactionClient,
    run_id: u64,
    prefix: &str,
    rng: &mut StdRng,
) -> Result<()> {
    let inode = 1u64;
    let ik = inode_key(run_id, inode);

    let old_key = format!("{prefix}inode-{inode}/old");
    let new_key = format!("{prefix}inode-{inode}/new");

    let old_file = PathBuf::from("/tmp/spike-c-old.bin");
    let new_file = PathBuf::from("/tmp/spike-c-new.bin");
    write_random_file(&old_file, 256 * 1024, rng)?;
    write_random_file(&new_file, 256 * 1024, rng)?;

    s3_put_object(BUCKET, &old_key, &old_file)?;
    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 0,
            obj_key: Some(old_key.clone()),
        },
    )
    .await?;

    prepare_upload(client, run_id, inode, 0, new_key.clone(), 256 * 1024, 60_000).await?;
    s3_put_object(BUCKET, &new_key, &new_file)?;

    // crash here: lifecycle exists, object exists, inode still points to old
    recovery_sweep(client, run_id, BUCKET).await?;

    // verify
    let mut txn = begin(client).await?;
    let inode_rec = get_inode(&mut txn, ik).await?;
    let _ = txn.rollback().await;

    if inode_rec.gen != 1 || inode_rec.obj_key.as_deref() != Some(&new_key) {
        return Err(anyhow!(
            "scenario1 verify failed: inode={inode_rec:?}, expected gen=1, obj_key=new"
        ));
    }
    if head_with_retry(BUCKET, &new_key).await? != true {
        return Err(anyhow!("scenario1 verify failed: new object missing"));
    }
    if head_with_retry(BUCKET, &old_key).await? != false {
        return Err(anyhow!("scenario1 verify failed: old object still exists"));
    }

    Ok(())
}

async fn scenario_crash_after_publish_before_delete(
    client: &TransactionClient,
    run_id: u64,
    prefix: &str,
    rng: &mut StdRng,
) -> Result<()> {
    let inode = 2u64;
    let ik = inode_key(run_id, inode);
    let lk = life_key(run_id, inode);

    let old_key = format!("{prefix}inode-{inode}/old");
    let new_key = format!("{prefix}inode-{inode}/new");

    let old_file = PathBuf::from("/tmp/spike-c2-old.bin");
    let new_file = PathBuf::from("/tmp/spike-c2-new.bin");
    write_random_file(&old_file, 128 * 1024, rng)?;
    write_random_file(&new_file, 128 * 1024, rng)?;

    s3_put_object(BUCKET, &old_key, &old_file)?;
    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 0,
            obj_key: Some(old_key.clone()),
        },
    )
    .await?;

    prepare_upload(client, run_id, inode, 0, new_key.clone(), 128 * 1024, 60_000).await?;
    s3_put_object(BUCKET, &new_key, &new_file)?;

    // publish (this should set lifecycle to Deleting old_key)
    let mut txn = begin(client).await?;
    let uploading = get_life(&mut txn, lk.clone()).await?.unwrap();
    let _ = txn.rollback().await;
    publish_or_abort(client, run_id, inode, uploading).await?;

    // crash here: old not deleted, lifecycle is Deleting
    if head_with_retry(BUCKET, &old_key).await? != true {
        return Err(anyhow!("scenario2 pre-recovery: old missing unexpectedly"));
    }

    recovery_sweep(client, run_id, BUCKET).await?;

    let mut txn = begin(client).await?;
    let inode_rec = get_inode(&mut txn, ik).await?;
    let life_after = get_life(&mut txn, lk).await?;
    let _ = txn.rollback().await;

    if inode_rec.gen != 1 || inode_rec.obj_key.as_deref() != Some(&new_key) {
        return Err(anyhow!("scenario2 verify failed: inode={inode_rec:?}"));
    }
    if life_after.is_some() {
        return Err(anyhow!(
            "scenario2 verify failed: lifecycle not cleared: {life_after:?}"
        ));
    }
    if head_with_retry(BUCKET, &old_key).await? != false {
        return Err(anyhow!("scenario2 verify failed: old still exists"));
    }

    Ok(())
}

async fn scenario_cas_failed_aborts_new_object(
    client: &TransactionClient,
    run_id: u64,
    prefix: &str,
    rng: &mut StdRng,
) -> Result<()> {
    let inode = 3u64;
    let ik = inode_key(run_id, inode);

    let old_key = format!("{prefix}inode-{inode}/old");
    let other_key = format!("{prefix}inode-{inode}/other");
    let new_key = format!("{prefix}inode-{inode}/new");

    let old_file = PathBuf::from("/tmp/spike-c3-old.bin");
    let other_file = PathBuf::from("/tmp/spike-c3-other.bin");
    let new_file = PathBuf::from("/tmp/spike-c3-new.bin");
    write_random_file(&old_file, 64 * 1024, rng)?;
    write_random_file(&other_file, 64 * 1024, rng)?;
    write_random_file(&new_file, 64 * 1024, rng)?;

    s3_put_object(BUCKET, &old_key, &old_file)?;
    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 0,
            obj_key: Some(old_key.clone()),
        },
    )
    .await?;

    prepare_upload(client, run_id, inode, 0, new_key.clone(), 64 * 1024, 60_000).await?;
    s3_put_object(BUCKET, &new_key, &new_file)?;

    // concurrent writer publishes something else
    s3_put_object(BUCKET, &other_key, &other_file)?;
    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 1,
            obj_key: Some(other_key.clone()),
        },
    )
    .await?;

    recovery_sweep(client, run_id, BUCKET).await?;

    let mut txn = begin(client).await?;
    let inode_rec = get_inode(&mut txn, ik).await?;
    let _ = txn.rollback().await;

    if inode_rec.gen != 1 || inode_rec.obj_key.as_deref() != Some(&other_key) {
        return Err(anyhow!("scenario3 verify failed: inode={inode_rec:?}"));
    }
    if head_with_retry(BUCKET, &new_key).await? != false {
        return Err(anyhow!("scenario3 verify failed: new key not deleted"));
    }

    Ok(())
}

async fn scenario_expired_upload_is_deleted(
    client: &TransactionClient,
    run_id: u64,
    prefix: &str,
    rng: &mut StdRng,
) -> Result<()> {
    let inode = 4u64;
    let ik = inode_key(run_id, inode);
    let lk = life_key(run_id, inode);

    let new_key = format!("{prefix}inode-{inode}/new");
    let new_file = PathBuf::from("/tmp/spike-c4-new.bin");
    write_random_file(&new_file, 32 * 1024, rng)?;

    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 0,
            obj_key: None,
        },
    )
    .await?;

    let now = now_ms();
    put_life_atomic(
        client,
        lk.clone(),
        Life::Uploading {
            expected_gen: 0,
            new_key: new_key.clone(),
            size: 32 * 1024,
            created_ms: now.saturating_sub(120_000),
            expires_ms: now.saturating_sub(1),
        },
    )
    .await?;

    // Ensure an object exists to be cleaned up.
    s3_put_object(BUCKET, &new_key, &new_file)?;

    recovery_sweep(client, run_id, BUCKET).await?;

    let mut txn = begin(client).await?;
    let life_after = get_life(&mut txn, lk).await?;
    let inode_rec = get_inode(&mut txn, ik).await?;
    let _ = txn.rollback().await;

    if life_after.is_some() {
        return Err(anyhow!(
            "scenario4 verify failed: lifecycle not cleared: {life_after:?}"
        ));
    }
    if inode_rec.gen != 0 || inode_rec.obj_key.is_some() {
        return Err(anyhow!("scenario4 verify failed: inode mutated: {inode_rec:?}"));
    }
    if head_with_retry(BUCKET, &new_key).await? != false {
        return Err(anyhow!("scenario4 verify failed: expired object not deleted"));
    }

    Ok(())
}

async fn scenario_publish_commit_conflict_retry(
    client: &TransactionClient,
    run_id: u64,
    prefix: &str,
    rng: &mut StdRng,
) -> Result<()> {
    let inode = 5u64;
    let ik = inode_key(run_id, inode);
    let lk = life_key(run_id, inode);

    let old_key = format!("{prefix}inode-{inode}/old");
    let new_key = format!("{prefix}inode-{inode}/new");

    let old_file = PathBuf::from("/tmp/spike-c5-old.bin");
    let new_file = PathBuf::from("/tmp/spike-c5-new.bin");
    write_random_file(&old_file, 64 * 1024, rng)?;
    write_random_file(&new_file, 64 * 1024, rng)?;

    s3_put_object(BUCKET, &old_key, &old_file)?;
    put_inode_atomic(
        client,
        ik.clone(),
        InodeRec {
            gen: 0,
            obj_key: Some(old_key.clone()),
        },
    )
    .await?;

    prepare_upload(client, run_id, inode, 0, new_key.clone(), 64 * 1024, 60_000).await?;
    s3_put_object(BUCKET, &new_key, &new_file)?;

    // Two concurrent publishers racing on the same lifecycle.
    let mut txn = begin(client).await?;
    let uploading = get_life(&mut txn, lk.clone()).await?.unwrap();
    let _ = txn.rollback().await;

    let barrier = std::sync::Arc::new(Barrier::new(3));
    let b1 = barrier.clone();
    let b2 = barrier.clone();

    let c1 = client.clone();
    let c2 = client.clone();
    let uploading1 = uploading.clone();
    let uploading2 = uploading.clone();

    let t1 = tokio::spawn(async move {
        b1.wait().await;
        publish_or_abort(&c1, run_id, inode, uploading1).await
    });

    let t2 = tokio::spawn(async move {
        b2.wait().await;
        publish_or_abort(&c2, run_id, inode, uploading2).await
    });

    barrier.wait().await;

    t1.await.context("join t1")??;
    t2.await.context("join t2")??;

    recovery_sweep(client, run_id, BUCKET).await?;

    let mut txn = begin(client).await?;
    let inode_rec = get_inode(&mut txn, ik).await?;
    let life_after = get_life(&mut txn, lk).await?;
    let _ = txn.rollback().await;

    if inode_rec.gen != 1 || inode_rec.obj_key.as_deref() != Some(&new_key) {
        return Err(anyhow!("scenario5 verify failed: inode={inode_rec:?}"));
    }
    if life_after.is_some() {
        return Err(anyhow!(
            "scenario5 verify failed: lifecycle not cleared: {life_after:?}"
        ));
    }

    Ok(())
}

async fn cleanup_tikv(client: &TransactionClient, run_id: u64) -> Result<usize> {
    let prefix = format!("{BENCH_PREFIX}/");
    let range = prefix_range(prefix.as_bytes());

    let mut txn = begin(client).await?;
    let iter = txn.scan(range, 50_000).await.map_err(|e| anyhow!(e))?;
    let mut keys: Vec<Key> = Vec::new();
    for KvPair(k, _v) in iter {
        let raw: Vec<u8> = k.into();
        let s = String::from_utf8_lossy(&raw);
        if s.contains(&format!("/{run_id}/")) {
            keys.push(raw.into());
        }
    }
    let _ = txn.rollback().await;

    if keys.is_empty() {
        return Ok(0);
    }

    let keys = std::sync::Arc::new(keys);
    let keys_len = keys.len();

    run_txn_with_retry(client, move |txn| {
        let keys = keys.clone();
        Box::pin(async move {
            for k in keys.iter() {
                txn.delete(k.clone()).await.map_err(|e| anyhow!(e))?;
            }
            Ok(())
        })
    })
    .await?;

    Ok(keys_len)
}

fn cleanup_s3_prefix(bucket: &str, prefix: &str) -> Result<()> {
    let uri = format!("s3://{bucket}/{prefix}");
    let (code, _stdout, stderr) = aws_output(&["s3", "rm", &uri, "--recursive"])?;
    if code != 0 {
        return Err(anyhow!("aws s3 rm failed: {stderr}"));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("# fs9 v2 Spike C: lifecycle / recovery failpoints\n");

    let mut config = Config::default().with_keyspace(KEYSPACE);
    if let (Ok(ca), Ok(cert), Ok(key)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        config = config.with_security(ca, cert, key);
    }

    let client = TransactionClient::new_with_config(vec![PD_ENDPOINT.to_string()], config)
        .await
        .context("connect tikv")?;

    let run_id: u64 = rand::random();
    let prefix = format!("bench/fs9v2/spike-c/{run_id}/");

    println!("pd: `{}`", PD_ENDPOINT);
    println!("keyspace: `{}`", KEYSPACE);
    println!("bucket: `{}`", BUCKET);
    println!("s3_prefix: `{}`", prefix);
    println!("run_id: `{}`\n", run_id);

    let mut rng = StdRng::seed_from_u64(0xDB9_0000_0000_000C);

    println!("## Scenarios\n");

    scenario_crash_after_upload_before_publish(&client, run_id, &prefix, &mut rng)
        .await
        .context("scenario1")?;
    println!("- scenario1: crash after upload, before publish: OK");

    scenario_crash_after_publish_before_delete(&client, run_id, &prefix, &mut rng)
        .await
        .context("scenario2")?;
    println!("- scenario2: crash after publish, before delete: OK");

    scenario_cas_failed_aborts_new_object(&client, run_id, &prefix, &mut rng)
        .await
        .context("scenario3")?;
    println!("- scenario3: CAS failure aborts uploaded new object: OK");

    scenario_expired_upload_is_deleted(&client, run_id, &prefix, &mut rng)
        .await
        .context("scenario4")?;
    println!("- scenario4: expired upload cleaned up: OK");

    scenario_publish_commit_conflict_retry(&client, run_id, &prefix, &mut rng)
        .await
        .context("scenario5")?;
    println!("- scenario5: concurrent publish attempts safe (conflict/no-op): OK\n");

    // Idempotence: sweep again when clean.
    recovery_sweep(&client, run_id, BUCKET)
        .await
        .context("recovery sweep again")?;

    let remaining = scan_lifecycles(&client, run_id).await?;
    if !remaining.is_empty() {
        return Err(anyhow!(
            "expected no remaining lifecycle keys, found: {remaining:?}"
        ));
    }

    println!("## Decisions (initial defaults)\n");
    println!(
        "- Commit retry: {COMMIT_MAX_RETRIES} attempts, exponential backoff base {COMMIT_BACKOFF_BASE_MS}ms"
    );
    println!(
        "- HEAD retry: {HEAD_MAX_RETRIES} attempts, exponential backoff base {HEAD_BACKOFF_BASE_MS}ms\n"
    );

    cleanup_s3_prefix(BUCKET, &prefix).context("cleanup s3")?;
    let deleted = cleanup_tikv(&client, run_id).await.context("cleanup tikv")?;
    println!("Cleanup: deleted {deleted} TiKV bench keys under run_id, removed s3 prefix");

    Ok(())
}
