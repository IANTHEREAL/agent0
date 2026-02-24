# get-db9

TypeScript SDK for [db9-server](https://github.com/db9/db9-server) — instant PostgreSQL-compatible databases on TiKV.

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

## Db9 Client

Full typed client for the API — databases, SQL, file storage, tokens, migrations, and more.

```typescript
import { createDb9Client } from 'get-db9/client';

// No token needed. Automatically anonymous-registers and saves credentials.
const client = createDb9Client();

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

## File Storage (fs9)

Each database comes with a built-in file system. All operations auto-refresh tokens on 401.

```typescript
const client = createDb9Client();
const dbId = 'your-database-id';

// Write a file (string or binary)
await client.fs.write(dbId, '/data/hello.txt', 'Hello, world!');

// Read file as text
const text = await client.fs.read(dbId, '/data/hello.txt');

// Read file as binary
const buffer = await client.fs.readBinary(dbId, '/data/image.png');

// List files in a directory
const files = await client.fs.list(dbId, '/data');

// List recursively
const allFiles = await client.fs.list(dbId, '/', { recursive: true });

// Check if file exists
const exists = await client.fs.exists(dbId, '/data/hello.txt');

// Get file metadata
const stat = await client.fs.stat(dbId, '/data/hello.txt');

// Create directory (recursive)
await client.fs.mkdir(dbId, '/data/nested/dir');

// Delete a file
await client.fs.remove(dbId, '/data/hello.txt');

// Get file system events (audit log)
const events = await client.fs.events(dbId, {
  limit: 50,
  path: '/data',
  type: 'write',
});
```

## Token Management

```typescript
const client = createDb9Client();

// Create a named API token
const token = await client.tokens.create({
  name: 'ci-deploy',
  expires_in_days: 90,
});
console.log(token.token); // Use this for CI/CD

// List all tokens
const tokens = await client.tokens.list();

// Revoke a token
await client.tokens.revoke(token.id);
```

## SQL Error Handling

SQL results include structured error details when queries fail:

```typescript
const result = await client.databases.sql(dbId, 'SELECT * FROM nonexistent');

if (result.error) {
  // result.error is a SqlErrorDetail object:
  // {
  //   message: "relation \"nonexistent\" does not exist",
  //   code: "42P01",         // PostgreSQL error code
  //   detail: "...",         // optional
  //   hint: "...",           // optional
  //   position: 15           // optional cursor position
  // }
  console.log(result.error.message);
  console.log(result.error.code);
}
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
| `timeout` | `number` | — | Request timeout in ms |
| `maxRetries` | `number` | `3` (max) | Retry count for failed requests |
| `retryDelay` | `number` | — | Delay between retries in ms |

### Db9 client options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `baseUrl` | `string` | Production URL | API endpoint |
| `token` | `string` | — | Bearer token (optional) |
| `fetch` | `FetchFn` | `globalThis.fetch` | Custom fetch |
| `credentialStore` | `CredentialStore` | `FileCredentialStore` | Load/save token |
| `timeout` | `number` | — | Request timeout in ms |
| `maxRetries` | `number` | `3` (max) | Retry count for failed requests |
| `retryDelay` | `number` | — | Delay between retries in ms |

## Zero-config client

```typescript
import { createDb9Client } from 'get-db9';

// No token needed! Auto-registers anonymously
const client = createDb9Client();
const db = await client.databases.create({ name: 'myapp' });
```

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

> **Note:** 401 errors are automatically retried with a fresh token for anonymous sessions. You typically won't see `Db9AuthError` unless the refresh itself fails.

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

## API Reference

### `client.auth`

| Method | Description |
|--------|-------------|
| `register(req)` | Create account with email/password |
| `login(req)` | Login and get bearer token |
| `me()` | Get current user profile |
| `anonymousRegister()` | Register anonymously (auto-called) |
| `anonymousRefresh(req)` | Refresh anonymous token |
| `getAnonymousSecret()` | Retrieve anonymous secret for token refresh |
| `ensureAnonymousSecret()` | Ensure anonymous secret is saved to credential store |
| `claim(req)` | Claim anonymous account with email/password |

### `client.tokens`

| Method | Description |
|--------|-------------|
| `create(req)` | Create a named API token (`{ name?, expires_in_days? }`) |
| `list()` | List all tokens |
| `revoke(tokenId)` | Revoke a token by ID |

### `client.databases`

| Method | Description |
|--------|-------------|
| `create(req)` | Create a new database |
| `list()` | List all databases |
| `get(id)` | Get database details |
| `delete(id)` | Delete a database |
| `resetPassword(id)` | Reset admin password |
| `observability(id)` | Get TPS, latency, connection stats |
| `sql(id, query)` | Execute SQL query (errors returned as `SqlErrorDetail`) |
| `sqlFile(id, content)` | Execute SQL from file content |
| `schema(id)` | Get schema metadata |
| `dump(id, req?)` | Export schema/data as SQL |
| `applyMigration(id, req)` | Apply a migration |
| `listMigrations(id)` | List applied migrations |
| `branch(id, req)` | Create a database branch |
| `users.list(id)` | List database users |
| `users.create(id, req)` | Create database user |
| `users.delete(id, username)` | Delete database user |

### `client.fs`

| Method | Description |
|--------|-------------|
| `read(dbId, path)` | Read file as text |
| `readBinary(dbId, path)` | Read file as ArrayBuffer |
| `write(dbId, path, content)` | Write file (string, ArrayBuffer, Uint8Array, or Blob) |
| `list(dbId, path, opts?)` | List directory contents (`{ recursive?: boolean }`) |
| `stat(dbId, path)` | Get file metadata |
| `exists(dbId, path)` | Check if file exists (returns boolean) |
| `mkdir(dbId, path)` | Create directory recursively |
| `remove(dbId, path)` | Delete a file |
| `events(dbId, opts?)` | Get file system events (`{ limit, offset, path, type }`) |

## Requirements

- Node.js >= 18 (native fetch)
- TypeScript >= 5.0 (for type exports)

## License

Apache-2.0
