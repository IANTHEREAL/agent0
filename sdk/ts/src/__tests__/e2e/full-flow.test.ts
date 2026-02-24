import { afterAll, describe, expect, it } from 'vitest';
import { createDb9Client } from '../../client';
import { MemoryCredentialStore } from '../../credentials';
import { apiUrl, e2eDatabaseOpsUp, e2eStackUp } from './setup';

function uniqueName(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function createDatabaseWithRetry(
  client: ReturnType<typeof createDb9Client>,
  prefix: string,
  maxAttempts = 5
) {
  let lastError: unknown;

  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    try {
      return await client.databases.create({ name: uniqueName(prefix) });
    } catch (error) {
      lastError = error;
      const message = error instanceof Error ? error.message : String(error);
      if (!message.includes('Failed to initialize database') || attempt === maxAttempts) {
        throw error;
      }
      await sleep(500 * attempt);
    }
  }

  throw lastError instanceof Error ? lastError : new Error(String(lastError));
}

describe.skipIf(!e2eStackUp || !e2eDatabaseOpsUp)('Anonymous Registration + Database CRUD', () => {
  const credentialStore = new MemoryCredentialStore();
  const client = createDb9Client({
    baseUrl: apiUrl,
    credentialStore,
  });

  let dbId: string | null = null;

  afterAll(async () => {
    if (!dbId) return;
    try {
      await client.databases.delete(dbId);
    } catch {}
  });

  it('creates database and performs SQL CRUD flow', async () => {
    const db = await createDatabaseWithRetry(client, 'e2e');
    dbId = db.id;

    expect(db.id).toBeTruthy();
    expect(db.name).toBeTruthy();
    expect(db.state).toBeTruthy();
    expect(db.admin_password).toBeTruthy();

    const dbs = await client.databases.list();
    expect(dbs.some((d) => d.id === db.id)).toBe(true);

    const fetched = await client.databases.get(db.id);
    expect(fetched.id).toBe(db.id);
    expect(fetched.name).toBe(db.name);
    expect(fetched.state).toBeTruthy();

    const selectOne = await client.databases.sql(db.id, 'SELECT 1 as n');
    expect(selectOne.error).toBeUndefined();
    expect(selectOne.columns.length).toBeGreaterThan(0);
    expect(selectOne.rows.length).toBeGreaterThan(0);

    const nIdx = selectOne.columns.findIndex((c) => c.name === 'n');
    expect(nIdx).toBeGreaterThanOrEqual(0);
    expect(String(selectOne.rows[0][nIdx])).toBe('1');

    const createRes = await client.databases.sql(
      db.id,
      'CREATE TABLE e2e_test (id INT, name TEXT)'
    );
    expect(createRes.error).toBeUndefined();

    const insertRes = await client.databases.sql(
      db.id,
      "INSERT INTO e2e_test VALUES (1, 'hello')"
    );
    expect(insertRes.error).toBeUndefined();

    const rowsRes = await client.databases.sql(
      db.id,
      'SELECT * FROM e2e_test ORDER BY id'
    );
    expect(rowsRes.error).toBeUndefined();
    expect(rowsRes.rows.length).toBe(1);

    const idIdx = rowsRes.columns.findIndex((c) => c.name === 'id');
    const nameIdx = rowsRes.columns.findIndex((c) => c.name === 'name');
    expect(idIdx).toBeGreaterThanOrEqual(0);
    expect(nameIdx).toBeGreaterThanOrEqual(0);
    expect(String(rowsRes.rows[0][idIdx])).toBe('1');
    expect(String(rowsRes.rows[0][nameIdx])).toBe('hello');
  });
});

describe.skipIf(!e2eStackUp || !e2eDatabaseOpsUp)('FS Operations', () => {
  const credentialStore = new MemoryCredentialStore();
  const client = createDb9Client({
    baseUrl: apiUrl,
    credentialStore,
  });

  let dbId: string | null = null;

  afterAll(async () => {
    if (!dbId) return;
    try {
      await client.databases.delete(dbId);
    } catch {}
  });

   it('performs fs write/read/exists/stat/list/remove flow', async () => {
     const db = await createDatabaseWithRetry(client, 'e2e');
     dbId = db.id;
 
     await client.fs.write(db.id, '/e2e-test.txt', 'Hello World');
 
     const text = await client.fs.read(db.id, '/e2e-test.txt');
     expect(text).toBe('Hello World');
 
     const existsBefore = await client.fs.exists(db.id, '/e2e-test.txt');
     expect(existsBefore).toBe(true);
 
     const stat = await client.fs.stat(db.id, '/e2e-test.txt');
     expect(stat.file_type).toBe('regular');
     expect(stat.size).toBeGreaterThan(0);
 
     // NOTE: mkdir is skipped — fs9-server bug: POST /{ns}/api/v1/mkdir returns 404 under namespaced paths
 
     const rootEntries = await client.fs.list(db.id, '/');
     expect(rootEntries.some((e) => e.path === '/e2e-test.txt')).toBe(true);
 
     await client.fs.remove(db.id, '/e2e-test.txt');
 
     const existsAfter = await client.fs.exists(db.id, '/e2e-test.txt');
     expect(existsAfter).toBe(false);
   });
});

describe.skipIf(!e2eStackUp)('Token Lifecycle', () => {
  const credentialStore = new MemoryCredentialStore();
  const client = createDb9Client({
    baseUrl: apiUrl,
    credentialStore,
  });

  let createdTokenId: string | null = null;

  afterAll(async () => {
    if (!createdTokenId) return;
    try {
      await client.tokens.revoke(createdTokenId);
    } catch {}
  });

   it('creates, lists, revokes token', async () => {
    const tokenName = uniqueName('e2e-test-token');
    const created = await client.tokens.create({
      name: tokenName,
      expires_in_days: 1,
    });

    createdTokenId = created.id;
    expect(created.id).toBeTruthy();
    expect(created.token).toBeTruthy();

    const beforeRevoke = await client.tokens.list();
    expect(beforeRevoke.some((t) => t.id === created.id)).toBe(true);

    await client.tokens.revoke(created.id);

    const afterRevoke = await client.tokens.list();
    expect(afterRevoke.some((t) => t.id === created.id)).toBe(false);

    createdTokenId = null;
  });
});

describe.skipIf(!e2eStackUp)('Anonymous Claim Flow', () => {
  const credentialStore = new MemoryCredentialStore();
  const client = createDb9Client({
    baseUrl: apiUrl,
    credentialStore,
  });

  it('registers anonymously, verifies identity, claims account', async () => {
    // Step 1: First API call triggers anonymous registration
    const me = await client.auth.me();
    expect(me.id).toBeTruthy();

    // Step 2: Claim the account with email + password
    const email = `e2e-claim-${Date.now()}@test.local`;
    const password = 'TestPass123!';
    const claimed = await client.auth.claim({ email, password });
    expect(claimed.claimed).toBe(true);
    expect(claimed.email).toBe(email);

    // Step 3: Verify identity changed after claim
    const meAfter = await client.auth.me();
    expect(meAfter.email).toBe(email);
  });
});
