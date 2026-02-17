# Configuration

pg-tikv is configured through environment variables.

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
| `PGTIKV_BOOTSTRAP_ADMIN_USER` | `admin` | Initial superuser name for bootstrapping |
| `PGTIKV_BOOTSTRAP_ADMIN_PASSWORD` | (unset) | Initial superuser password for bootstrapping (required when no superuser exists yet) |
| `PGTIKV_DEV` | `false` | Dev-only escape hatch (legacy insecure bootstrap) |
| `PGTIKV_INSECURE` | `false` | Explicit insecure posture escape hatch |
| `PGTIKV_TOKIO_STACK_MB` | `4` | Tokio worker thread stack size (MB) |

## Examples

### Basic Configuration

```bash
PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=<password> ./target/release/pg-tikv
```

Uses all defaults:
- Connects to PD at `127.0.0.1:2379`
- Listens on port `5433`
- Uses `default` keyspace

### Custom PD Endpoints

```bash
PD_ENDPOINTS=10.0.0.1:2379,10.0.0.2:2379,10.0.0.3:2379 \
PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=<password> \
./target/release/pg-tikv
```

### Custom Port

```bash
PG_PORT=5432 PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=<password> ./target/release/pg-tikv
```

### Full Production Example

```bash
PD_ENDPOINTS=pd1.example.com:2379,pd2.example.com:2379 \
PG_PORT=5432 \
PG_KEYSPACE=production \
PGTIKV_BOOTSTRAP_ADMIN_PASSWORD=<strong_password> \
PG_TLS_CERT=/path/to/server.crt \
PG_TLS_KEY=/path/to/server.key \
PG_REQUIRE_TLS=1 \
./target/release/pg-tikv
```

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

pg-tikv uses the `tracing` crate for logging. Log level is set to INFO by default.

### Log Output

```
INFO pg-tikv starting up...
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

pg-tikv doesn't currently implement connection limits. Each connection spawns a tokio task.

### Query Limits

No built-in query timeout or result size limits. These should be implemented at the application level.

## Health Checks

pg-tikv responds to PostgreSQL protocol, so standard PostgreSQL health checks work:

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
COPY --from=builder /app/target/release/pg-tikv /usr/local/bin/
EXPOSE 5433
CMD ["pg-tikv"]
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

  pg-tikv:
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

Create `/etc/systemd/system/pg-tikv.service`:

```ini
[Unit]
Description=pg-tikv PostgreSQL-compatible TiKV frontend
After=network.target

[Service]
Type=simple
User=pgtikv
Group=pgtikv
Environment=PD_ENDPOINTS=127.0.0.1:2379
Environment=PG_PORT=5433
ExecStart=/usr/local/bin/pg-tikv
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable pg-tikv
sudo systemctl start pg-tikv
```
