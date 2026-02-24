# db9 Example: Trigger + HTTP Extension Webhook

This example demonstrates how to use **db9's AFTER triggers** combined with the **HTTP extension** to automatically send webhook notifications whenever a table is modified.

When a row is inserted, updated, or deleted in the `products` table, an AFTER trigger fires and:
1. Calls `extensions.http_post()` to POST a JSON payload to a webhook URL
2. Logs the HTTP response status into a `webhook_log` table

## Architecture

```
 SQLAlchemy App                          db9 Server
 ─────────────                          ────────────
  INSERT INTO products ──────────────▶  Execute INSERT
                                             │
                                        Queue AFTER trigger
                                             │
                                        Trigger Worker (async)
                                             │
                                     ┌───────┴────────┐
                                     │ Trigger Body:   │
                                     │ http_post(url)  │──────▶  Webhook Receiver
                                     │ INSERT INTO     │           (HTTP Server)
                                     │  webhook_log    │
                                     └────────────────┘
```

## Prerequisites

- **db9** running with the following environment variables:

  ```bash
  # Required: enable AFTER trigger background worker
  DB9_TRIGGER_ENABLED=true

  # Required: allow HTTP (not just HTTPS) for local webhook receiver
  DB9_HTTP_ALLOW_INSECURE=true
  ```

- **Python 3.10+**

## Quick Start

```bash
# 1. Start TiKV
tiup playground --mode tikv-slim

# 2. Start db9 (in another terminal)
DB9_TRIGGER_ENABLED=true DB9_HTTP_ALLOW_INSECURE=true cargo run

# 3. Run the demo (installs dependencies automatically)
cd examples/02_python_trigger_webhook
./run_demo.sh postgresql://admin:admin@127.0.0.1:5433/postgres

# Or manually:
uv sync
uv run python run.py postgresql://admin:admin@127.0.0.1:5433/postgres
```

## Expected Output

```
============================================================
db9 Trigger + HTTP Extension Webhook Demo
============================================================

[1] Starting webhook receiver on http://127.0.0.1:8765/webhook ...
    Webhook receiver is running.

[2] Connecting to db9 at postgresql://admin:admin@127.0.0.1:5433/postgres ...
    Setting up tables, HTTP extension, and triggers ...
    Done. Tables: products, webhook_log
    Triggers: trg_product_insert, trg_product_update, trg_product_delete

[3] Inserting products ...
    Inserted 3 products.
    Waiting for AFTER triggers to fire ...

[4] Updating 'Laptop' price to 899.99 ...

[5] Deleting 'Coffee Mug' ...

============================================================
Results
============================================================

-- Products table --
   <Product(id=1, name='Laptop', price=899.99)>
   <Product(id=2, name='Headphones', price=49.99)>

-- Webhook log (from trigger) --
   id=1  event=INSERT  table=products  payload={"event":"INSERT","id":1,...}  http_status=200
   id=2  event=INSERT  table=products  payload={"event":"INSERT","id":2,...}  http_status=200
   id=3  event=INSERT  table=products  payload={"event":"INSERT","id":3,...}  http_status=200
   id=4  event=UPDATE  table=products  payload={"event":"UPDATE","id":1,...}  http_status=200
   id=5  event=DELETE  table=products  payload={"event":"DELETE","id":3,...}  http_status=200

-- Webhooks received by HTTP server --
   #1: {"event": "INSERT", "id": 1, "name": "Laptop", "price": 999.99}
   #2: {"event": "INSERT", "id": 2, "name": "Headphones", "price": 49.99}
   #3: {"event": "INSERT", "id": 3, "name": "Coffee Mug", "price": 12.50}
   #4: {"event": "UPDATE", "id": 1, "name": "Laptop", "price": 899.99}
   #5: {"event": "DELETE", "id": 3, "name": "Coffee Mug"}
```

## How It Works

### 1. Enable the HTTP Extension

```sql
CREATE EXTENSION IF NOT EXISTS http;
```

### 2. Create Trigger Functions

Each trigger function uses `extensions.http_post()` to send a JSON payload to the webhook URL, and logs the HTTP status into `webhook_log`:

```sql
CREATE OR REPLACE FUNCTION notify_product_insert()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO webhook_log (event_type, table_name, payload, http_status)
    SELECT
        'INSERT',
        'products',
        '{"event":"INSERT","id":' || NEW.id::text || ',"name":"' || NEW.name || '"}',
        status
    FROM extensions.http_post(
        'http://localhost:8765/webhook',
        '{"event":"INSERT","id":' || NEW.id::text || ',"name":"' || NEW.name || '"}',
        'application/json'
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
```

### 3. Attach AFTER Triggers

```sql
CREATE TRIGGER trg_product_insert
    AFTER INSERT ON products
    FOR EACH ROW EXECUTE FUNCTION notify_product_insert();
```

### Key db9 Features Used

| Feature | Description |
|---------|-------------|
| `AFTER` triggers | Fire asynchronously via db9's background trigger worker |
| `extensions.http_post()` | Built-in HTTP extension for making outbound HTTP requests |
| `NEW.column` / `OLD.column` | Access the inserted/updated/deleted row data inside trigger functions |
| `webhook_log` table | Persist the HTTP call result for auditing |

## Notes

- **AFTER triggers in db9 are asynchronous**: they are queued and processed by a background worker, so there is a small delay between the DML operation and the webhook call. The demo uses `time.sleep(1)` to wait for processing.
- **HTTP extension requires superuser**: the db9 connection must use a superuser account (default `admin`).
- **HTTPS by default**: db9 only allows HTTPS URLs unless `DB9_HTTP_ALLOW_INSECURE=true` is set. For production, use an HTTPS webhook endpoint.
- **db9 does not support `TG_OP`/`TG_TABLE_NAME`**: unlike PostgreSQL, db9 trigger functions cannot access these special variables. This example creates separate functions per event type as a workaround.

## Project Structure

```
02_python_trigger_webhook/
├── README.md               ← This file
├── pyproject.toml           ← Project metadata and dependencies
├── run_demo.sh              ← Quick start script (uses uv)
├── .env.example             ← Environment variable template
├── run.py                   ← Entry point
└── trigger_webhook/
    ├── __init__.py
    ├── models.py            ← SQLAlchemy models (Product, WebhookLog)
    ├── database.py          ← DB setup: tables, extension, triggers
    └── main.py              ← Demo logic + embedded webhook receiver
```
