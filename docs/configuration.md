# Configuration

db9-server is configured through environment variables.

## Environment Variables

This document is a convenience overview. The authoritative list of config keys + defaults is `docs/sot/ops-config.md`.

| Variable | Default | Description |
|----------|---------|-------------|
| `PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD endpoints (comma-separated) |
| `PG_PORT` | `5433` | PostgreSQL protocol listen port |
| `PG_LISTEN_ADDR` | `127.0.0.1` | Listen address (loopback by default) |
| `PG_KEYSPACE` | `default` | Default keyspace when not specified in username |
| `PG_TLS_CERT` | (unset) | TLS cert path (PEM); enable TLS only when both cert+key are set |
| `PG_TLS_KEY` | (unset) | TLS key path (PEM; PKCS#8 or RSA) |
| `PG_REQUIRE_TLS` | `false` | Require TLS for all pgwire connections |
| `DB9_AUTH_MODE` | `password` | Authentication mode: `password` (legacy), `both` (password + token), `token` (token only) |
| `DB9_AUTH_JWKS_URL` | (unset) | JWT verification via remote JWKS (preferred) |
| `DB9_AUTH_JWT_PUBLIC_KEY` | (unset) | JWT verification via RSA public key (PEM) |
| `DB9_AUTH_JWT_ALGORITHM` | `RS256` | Allowed JWT algorithms (comma-separated). Default: `RS256`. |
| `DB9_AUTH_ISSUER` | (unset) | Optional JWT issuer constraint (single value; used as fallback when `DB9_AUTH_ISSUERS` is unset) |
| `DB9_AUTH_ISSUERS` | (unset) | Optional JWT issuer constraint accepting multiple values (comma-separated, e.g. `https://auth9.example,https://legacy.example`). When set, overrides `DB9_AUTH_ISSUER`. |
| `DB9_AUTH_AUDIENCE` | `db9-server` | JWT audience constraint |
| `DB9_AUTH_CONNECT_KEY_INTROSPECT_URL` | (unset) | Connect-key introspection endpoint URL |
| `DB9_AUTH_CONNECT_KEY_INTROSPECT_API_KEY` | (unset) | Optional `X-API-Key` header for connect-key introspection |
| `DB9_BOOTSTRAP_ADMIN_USER` | `admin` | Initial superuser name for bootstrapping |
| `DB9_BOOTSTRAP_ADMIN_PASSWORD` | (unset) | Initial superuser password for bootstrapping (required when no superuser exists yet) |
| `DB9_DEV` | `false` | Dev-only escape hatch (legacy insecure bootstrap) |
| `DB9_INSECURE` | `false` | Explicit insecure posture escape hatch |
| `DB9_TOKIO_STACK_MB` | `4` | Tokio worker thread stack size (MB) |
| `HNSW_S3_BUCKET` | (unset) | S3 bucket for HNSW graph offload. When set, HNSW graphs are stored in S3 instead of TiKV, removing the 8 MB size limit. When unset, behavior is unchanged (TiKV-only). |
| `HNSW_S3_REGION` | (unset) | S3 region. Falls back to `AWS_REGION` / `AWS_DEFAULT_REGION`. |
| `HNSW_S3_ENDPOINT` | (unset) | S3-compatible endpoint URL (e.g. `http://minio:9000`). |
| `HNSW_S3_PREFIX` | `hnsw` | S3 key prefix for graph objects. |
| `HNSW_S3_FORCE_PATH_STYLE` | `false` | Use path-style URLs (required for MinIO). |
| `HNSW_CACHE_MAX_ENTRIES` | `64` | Max number of cached HNSW graph files (LRU). Set higher for deployments with many hot indexes. |
| `HNSW_CACHE_DIR` | `/tmp/db9_hnsw_cache` | Base directory for cached HNSW graph files. db9 uses a `db9_hnsw_cache/` subdirectory under this path; use a dedicated volume for high-QPS workloads. |

## Examples

### Basic Configuration

```bash
DB9_BOOTSTRAP_ADMIN_PASSWORD=<password> ./target/release/db9-server
```

Uses all defaults:
- Connects to PD at `127.0.0.1:2379`
- Listens on port `5433`
- Uses `default` keyspace

### Custom PD Endpoints

```bash
PD_ENDPOINTS=10.0.0.1:2379,10.0.0.2:2379,10.0.0.3:2379 \
DB9_BOOTSTRAP_ADMIN_PASSWORD=<password> \
./target/release/db9-server
```

### Custom Port

```bash
PG_PORT=5432 DB9_BOOTSTRAP_ADMIN_PASSWORD=<password> ./target/release/db9-server
```

### Full Production Example

```bash
PD_ENDPOINTS=pd1.example.com:2379,pd2.example.com:2379 \
PG_PORT=5432 \
PG_KEYSPACE=production \
DB9_BOOTSTRAP_ADMIN_PASSWORD=<strong_password> \
PG_TLS_CERT=/path/to/server.crt \
PG_TLS_KEY=/path/to/server.key \
PG_REQUIRE_TLS=1 \
./target/release/db9-server
```

### HNSW S3 Offload (Optional)

Stores HNSW vector index graphs in S3, removing the 8 MB TiKV value size limit.
Without this, indexes freeze at ~1,300 rows for VECTOR(1536) embeddings.

```bash
# AWS S3
HNSW_S3_BUCKET=my-hnsw-graphs \
HNSW_S3_REGION=us-east-1 \
DB9_BOOTSTRAP_ADMIN_PASSWORD=<password> \
./target/release/db9-server
```

```bash
# MinIO / S3-compatible
HNSW_S3_BUCKET=hnsw \
HNSW_S3_ENDPOINT=http://minio:9000 \
HNSW_S3_FORCE_PATH_STYLE=true \
DB9_BOOTSTRAP_ADMIN_PASSWORD=<password> \
./target/release/db9-server
```

S3 credentials use the standard AWS chain (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`,
EC2 Instance Profile, or ECS Task Role). Required IAM permissions:
`s3:PutObject`, `s3:GetObject`, `s3:DeleteObject`, `s3:ListBucket`.

When `HNSW_S3_BUCKET` is not set, HNSW behavior is unchanged (TiKV-only with
8 MB frozen guard). Existing indexes migrate automatically on next merge cycle.

## TiKV Configuration

### Single Node (Development)

```bash
tiup playground --mode tikv-slim
```

### With Keyspace Support

Create `/tmp/tikv.toml`:

```toml
[storage]
api-version = 2
enable-ttl = true
```

Start with config:

```bash
tiup playground --mode tikv-slim --kv.config /tmp/tikv.toml
```

### Production Cluster

For production, deploy a proper TiKV cluster:

```bash
tiup cluster deploy mycluster v8.5.4 topology.yaml
tiup cluster start mycluster
```

See [TiKV documentation](https://tikv.org/docs/) for cluster deployment.

## Connection Configuration

### Client Connection String

```
postgresql://username:password@host:port/database
```

Examples:

```bash
# Default keyspace
psql "postgresql://admin:<password>@localhost:5433/postgres"

# With keyspace in username
psql "postgresql://tenant_a.admin:<password>@localhost:5433/postgres"
```

### Driver Configuration

**Python (psycopg2)**:

```python
import psycopg2

conn = psycopg2.connect(
    host="localhost",
    port=5433,
    user="tenant_a.admin",
    password="<password>",
    database="postgres"
)
```

**Node.js (pg)**:

```javascript
const { Client } = require('pg');

const client = new Client({
    host: 'localhost',
    port: 5433,
    user: 'tenant_a.admin',
    password: '<password>',
    database: 'postgres'
});
```

**Go (pgx)**:

```go
import "github.com/jackc/pgx/v5"

conn, err := pgx.Connect(context.Background(), 
    "postgres://tenant_a.admin:<password>@localhost:5433/postgres")
```

**Rust (tokio-postgres)**:

```rust
use tokio_postgres::NoTls;

let (client, connection) = tokio_postgres::connect(
    "host=localhost port=5433 user=tenant_a.admin password=<password> dbname=postgres",
    NoTls,
).await?;
```

## Logging

db9-server uses the `tracing` crate for logging. Log level is set to INFO by default.

### Log Output

```
INFO db9-server starting up...
INFO PD endpoints: 127.0.0.1:2379
INFO PostgreSQL port: 5433
INFO PostgreSQL listen addr: 127.0.0.1
INFO Default keyspace: default
INFO Password authentication: enabled (via AuthManager)
INFO PostgreSQL server listening on 127.0.0.1:5433
INFO New connection from 127.0.0.1:54321
INFO Extracted keyspace 'tenant_a' from username 'tenant_a.admin'
INFO Authentication successful for user 'admin' with keyspace Some("tenant_a")
INFO Received query: SELECT * FROM users
```

### Custom Log Level

Currently log level is hardcoded. For custom logging, modify `src/main.rs`:

```rust
let subscriber = FmtSubscriber::builder()
    .with_max_level(Level::DEBUG)  // or TRACE, WARN, ERROR
    .finish();
```

## Resource Limits

### Connection Limits

db9-server doesn't currently implement connection limits. Each connection spawns a tokio task.

### Query Limits

No built-in query timeout or result size limits. These should be implemented at the application level.

## Health Checks

db9-server responds to PostgreSQL protocol, so standard PostgreSQL health checks work:

```bash
# Simple connectivity check
pg_isready -h localhost -p 5433

# Query-based check
psql -h localhost -p 5433 -U admin -c "SELECT 1"
```

## Docker Configuration

### Dockerfile

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/db9-server /usr/local/bin/
EXPOSE 5433
CMD ["db9-server"]
```

### Docker Compose

```yaml
version: '3.8'
services:
  pd:
    image: pingcap/pd:latest
    ports:
      - "2379:2379"
    command:
      - --name=pd
      - --client-urls=http://0.0.0.0:2379
      - --peer-urls=http://0.0.0.0:2380

  tikv:
    image: pingcap/tikv:latest
    depends_on:
      - pd
    command:
      - --pd-endpoints=pd:2379
      - --addr=0.0.0.0:20160

  db9-server:
    build: .
    depends_on:
      - tikv
    ports:
      - "5433:5433"
    environment:
      - PD_ENDPOINTS=pd:2379
      - PG_PORT=5433
```

## Systemd Service

Create `/etc/systemd/system/db9-server.service`:

```ini
[Unit]
Description=db9-server PostgreSQL-compatible TiKV frontend
After=network.target

[Service]
Type=simple
User=db9
Group=db9
Environment=PD_ENDPOINTS=127.0.0.1:2379
Environment=PG_PORT=5433
ExecStart=/usr/local/bin/db9-server
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable db9-server
sudo systemctl start db9-server
```
