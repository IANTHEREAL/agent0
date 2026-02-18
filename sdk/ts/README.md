# get-db9

TypeScript SDK for [pg-tikv](https://github.com/pgtikv/pg-tikv) — instant PostgreSQL-compatible databases on TiKV.

## Install

```bash
npm install get-db9
```

## Quick Start

### One-liner: Get a database instantly

```typescript
import { instantDatabase } from 'get-db9';

const db = await instantDatabase();
console.log(db.connectionString);
// postgresql://tenant.admin:password@host:5433/postgres
```

### With seeding

```typescript
const db = await instantDatabase({
  name: 'myapp',
  seed: 'CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT)',
});
```

## Customer API

Full typed client for the Customer API (register, databases, SQL, migrations).

```typescript
import { createCustomerClient } from 'get-db9/customer';

const client = createCustomerClient({ token: 'your-token' });

// Create a database
const db = await client.databases.create({ name: 'myapp' });

// Execute SQL
const result = await client.databases.sql(db.id, 'SELECT * FROM users');
console.log(result.columns, result.rows);

// Schema inspection
const schema = await client.databases.schema(db.id);

// Migrations
await client.databases.applyMigration(db.id, {
  name: 'add_users',
  sql: 'CREATE TABLE users (id SERIAL PRIMARY KEY)',
  checksum: 'abc123',
});
```

## Admin API

Full typed client for the Admin API (tenant management, batch operations, audit).

```typescript
import { createAdminClient } from 'get-db9/admin';

const admin = createAdminClient({ apiKey: 'your-api-key' });

// List tenants
const { items } = await admin.tenants.list({ state: 'ACTIVE' });

// Create tenant
const tenant = await admin.tenants.create();
console.log(tenant.connection_string);

// Batch operations
const batch = await admin.tenants.batchCreate({ count: 5 });
```

## Configuration

### instantDatabase options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `name` | `string` | `'default'` | Database name |
| `baseUrl` | `string` | Production URL | API endpoint |
| `fetch` | `FetchFn` | `globalThis.fetch` | Custom fetch |
| `credentialStore` | `CredentialStore` | `FileCredentialStore` | Credential storage |
| `seed` | `string` | — | SQL to run after creation |
| `seedFile` | `string` | — | SQL file content to run |

### Customer client options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `baseUrl` | `string` | Production URL | API endpoint |
| `token` | `string` | — | Bearer token |
| `fetch` | `FetchFn` | `globalThis.fetch` | Custom fetch |
| `credentialStore` | `CredentialStore` | — | Auto-load token |

### Admin client options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `baseUrl` | `string` | Production URL | API endpoint |
| `apiKey` | `string` | — | X-API-Key header |
| `fetch` | `FetchFn` | `globalThis.fetch` | Custom fetch |

## Error Handling

```typescript
import { Db9Error, Db9AuthError, Db9NotFoundError } from 'get-db9';

try {
  await client.databases.get('nonexistent');
} catch (err) {
  if (err instanceof Db9NotFoundError) {
    console.log('Database not found');
  } else if (err instanceof Db9AuthError) {
    console.log('Authentication failed');
  } else if (err instanceof Db9Error) {
    console.log(`API error ${err.statusCode}: ${err.message}`);
  }
}
```

## Credential Storage

Credentials are stored in `~/.db9/credentials` (TOML format), shared with the db9 CLI.

```typescript
import { FileCredentialStore, MemoryCredentialStore } from 'get-db9';

// File-based (default, shared with CLI)
const fileStore = new FileCredentialStore();

// Custom path
const customStore = new FileCredentialStore('/path/to/credentials');

// In-memory (for testing or serverless)
const memStore = new MemoryCredentialStore();
```

## Requirements

- Node.js >= 18 (native fetch)
- TypeScript >= 5.0 (for type exports)

## License

Apache-2.0
