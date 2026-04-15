# fs-plane POC Validation

Validates 3 critical assumptions before full fs-plane implementation.

## Prerequisites

- JuiceFS fork cloned: `gh repo clone db9-ai/juicefs /tmp/juicefs`
- Access to TiKV dev cluster (PD endpoint)
- Access to S3 bucket with credentials
- A pre-formatted JuiceFS volume

## Setup

```bash
# Format a test volume (once)
cd /tmp/juicefs
go run . format "tikv://pd:2379?keyspace=jfs_t_poc_test&gc-interval=0" \
  --storage s3 --bucket https://your-bucket.s3.amazonaws.com \
  --access-key $AWS_ACCESS_KEY --secret-key $AWS_SECRET_KEY \
  --trash-days 7 --block-size 4096 \
  poc-test

# Copy POC code into JuiceFS repo (avoids dependency hell)
mkdir -p /tmp/juicefs/cmd/poc
cp main.go /tmp/juicefs/cmd/poc/
```

## Run

```bash
cd /tmp/juicefs

# Validation 1 & 2: Flush latency + memory
go run ./cmd/poc/ \
  --meta "tikv://pd:2379?keyspace=jfs_t_poc_test&gc-interval=0" \
  --iterations 1000

# Validation 3: TiKV BR backup by keyspace (manual)
# If using ?keyspace= (API V2):
tikv-br backup full --pd pd:2379 --keyspace jfs_t_poc_test -s "s3://backup-bucket/poc-test"
tikv-br restore full --pd pd:2379 --keyspace jfs_t_poc_test_clone -s "s3://backup-bucket/poc-test"

# Then verify the clone:
go run . mount "tikv://pd:2379?keyspace=jfs_t_poc_test_clone&gc-interval=0" /mnt/clone
ls /mnt/clone  # should see files from the original volume
```

## What each validation answers

| # | Question | Pass criteria | If fails |
|---|---|---|---|
| 1 | Flush P99 latency for 4KB write | < 50ms | Need buffered mode + explicit Sync; architecture changes |
| 2 | Per-volume memory overhead | < 25MB/vol (512MB / 20 vols) | Need on-demand volume loading/eviction or separate service |
| 3 | TiKV BR can backup/restore by keyspace | Clone reads correct data | Need key-range backup or redesign backup approach |
