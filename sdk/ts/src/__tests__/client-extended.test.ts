import { describe, it, expect } from 'vitest';
import { createDb9Client } from '../client';
import type { FetchFn } from '../http';

function capturingFetch(status: number, body?: unknown) {
  const calls: { url: string; init?: RequestInit }[] = [];
  const fn: FetchFn = async (url, init) => {
    calls.push({ url: url.toString(), init });
    return new Response(body ? JSON.stringify(body) : null, {
      status,
      headers: { 'Content-Type': 'application/json' },
    });
  };
  return { fn, calls };
}

const BASE = 'http://test:8090/api';
const TOKEN = 'test-token';

function authedClient(fetch: FetchFn) {
  return createDb9Client({ baseUrl: BASE, fetch, token: TOKEN });
}

function expectAuth(calls: { url: string; init?: RequestInit }[]) {
  for (const call of calls) {
    expect(
      (call.init?.headers as Record<string, string>)['Authorization']
    ).toBe(`Bearer ${TOKEN}`);
  }
}

describe('databases – schema & dump', () => {
  it('schema() → GET /customer/databases/:id/schema', async () => {
    const { fn, calls } = capturingFetch(200, {
      tables: [
        {
          name: 'users',
          schema: 'public',
          columns: [
            { name: 'id', type: 'integer', nullable: false },
            { name: 'name', type: 'text', nullable: true },
          ],
        },
      ],
      views: [{ name: 'active_users', schema: 'public' }],
    });
    const client = authedClient(fn);

    const res = await client.databases.schema('db1');

    expect(res.tables).toHaveLength(1);
    expect(res.tables[0].name).toBe('users');
    expect(res.tables[0].columns).toHaveLength(2);
    expect(res.views[0].name).toBe('active_users');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/schema`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('dump() without body → POST /customer/databases/:id/dump', async () => {
    const { fn, calls } = capturingFetch(200, {
      sql: 'CREATE TABLE users ...',
      object_count: 3,
    });
    const client = authedClient(fn);

    const res = await client.databases.dump('db1');

    expect(res.sql).toContain('CREATE TABLE');
    expect(res.object_count).toBe(3);
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/dump`);
    expect(calls[0].init?.method).toBe('POST');
    expectAuth(calls);
  });

  it('dump() with ddl_only → POST /customer/databases/:id/dump with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      sql: 'CREATE TABLE ...',
      object_count: 1,
    });
    const client = authedClient(fn);

    const res = await client.databases.dump('db1', { ddl_only: true });

    expect(res.object_count).toBe(1);
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/dump`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ ddl_only: true }));
    expectAuth(calls);
  });
});

describe('databases – migrations', () => {
  it('applyMigration() → POST /customer/databases/:id/migrations with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      status: 'applied',
      name: 'v1',
    });
    const client = authedClient(fn);

    const req = { name: 'v1', sql: 'CREATE TABLE t(id INT)', checksum: 'abc123' };
    const res = await client.databases.applyMigration('db1', req);

    expect(res.status).toBe('applied');
    expect(res.name).toBe('v1');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/migrations`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify(req));
    expectAuth(calls);
  });

  it('listMigrations() → GET /customer/databases/:id/migrations', async () => {
    const { fn, calls } = capturingFetch(200, [
      {
        name: 'v1',
        checksum: 'abc123',
        applied_at: '2026-01-01T00:00:00Z',
        sql_preview: 'CREATE TABLE ...',
      },
    ]);
    const client = authedClient(fn);

    const res = await client.databases.listMigrations('db1');

    expect(res).toHaveLength(1);
    expect(res[0].name).toBe('v1');
    expect(res[0].checksum).toBe('abc123');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/migrations`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });
});

describe('databases – branching', () => {
  it('branch() → POST /customer/databases/:id/branch with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'db2',
      name: 'dev',
      state: 'ACTIVE',
      created_at: '2026-01-01T00:00:00Z',
    });
    const client = authedClient(fn);

    const res = await client.databases.branch('db1', { name: 'dev' });

    expect(res.id).toBe('db2');
    expect(res.name).toBe('dev');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/branch`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ name: 'dev' }));
    expectAuth(calls);
  });
});

describe('databases – user management', () => {
  it('users.list() → GET /customer/databases/:id/users', async () => {
    const { fn, calls } = capturingFetch(200, [
      {
        name: 'admin',
        is_superuser: true,
        can_login: true,
        can_create_db: true,
        can_create_role: true,
      },
    ]);
    const client = authedClient(fn);

    const res = await client.databases.users.list('db1');

    expect(res).toHaveLength(1);
    expect(res[0].name).toBe('admin');
    expect(res[0].is_superuser).toBe(true);
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/users`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('users.create() → POST /customer/databases/:id/users with body', async () => {
    const { fn, calls } = capturingFetch(200, { message: 'User created' });
    const client = authedClient(fn);

    const res = await client.databases.users.create('db1', {
      username: 'app',
      password: 'pass',
    });

    expect(res.message).toBe('User created');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/users`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ username: 'app', password: 'pass' })
    );
    expectAuth(calls);
  });

   it('users.delete() → DELETE /customer/databases/:id/users/:username', async () => {
    const { fn, calls } = capturingFetch(200, { message: 'User deleted' });
    const client = authedClient(fn);

    const res = await client.databases.users.delete('db1', 'app');

    expect(res.message).toBe('User deleted');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/users/app`);
    expect(calls[0].init?.method).toBe('DELETE');
    expectAuth(calls);
  });
});

describe('databases – SQL error wrapping', () => {
  it('wraps string error into SqlErrorDetail object', async () => {
    const { fn, calls } = capturingFetch(200, {
      error: 'relation "users" does not exist',
      columns: [],
      rows: [],
    });
    const client = authedClient(fn);

    const result = await client.databases.sql('db1', 'SELECT * FROM users');

    expect(result.error).toEqual({ message: 'relation "users" does not exist' });
    expect(calls[0].init?.method).toBe('POST');
  });

  it('preserves undefined error on success', async () => {
    const { fn } = capturingFetch(200, {
      columns: ['id', 'name'],
      rows: [[1, 'Alice']],
    });
    const client = authedClient(fn);

    const result = await client.databases.sql('db1', 'SELECT * FROM users');

    expect(result.error).toBeUndefined();
    expect(result.columns).toEqual(['id', 'name']);
  });

  it('sqlFile also wraps string error', async () => {
    const { fn } = capturingFetch(200, {
      error: 'syntax error at position 1',
      columns: [],
      rows: [],
    });
    const client = authedClient(fn);

    const result = await client.databases.sqlFile('db1', 'INVALID SQL');

    expect(result.error).toEqual({ message: 'syntax error at position 1' });
  });

  it('parses JSON error string into SqlErrorDetail', async () => {
    const { fn } = capturingFetch(200, {
      error: '{"message":"fail","code":"42P01"}',
      columns: [],
      rows: [],
    });
    const client = authedClient(fn);

    const result = await client.databases.sql('db1', 'SELECT 1');

    expect(result.error).toEqual({ message: 'fail', code: '42P01' });
  });

  it('parses PostgreSQL-style error with SQLSTATE', async () => {
    const { fn } = capturingFetch(200, {
      error: 'ERROR: relation "users" does not exist (SQLSTATE 42P01)',
      columns: [],
      rows: [],
    });
    const client = authedClient(fn);

    const result = await client.databases.sql('db1', 'SELECT 1');

    expect(result.error).toMatchObject({
      message: expect.stringContaining('relation "users" does not exist'),
      code: '42P01',
    });
  });

  it('sqlFile also parses JSON error string', async () => {
    const { fn } = capturingFetch(200, {
      error: '{"message":"syntax error","detail":"near SELECT"}',
      columns: [],
      rows: [],
    });
    const client = authedClient(fn);

    const result = await client.databases.sqlFile('db1', 'BAD SQL');

    expect(result.error).toEqual({ message: 'syntax error', detail: 'near SELECT' });
  });
});

describe('tokens', () => {
  it('create() → POST /customer/tokens with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'tok_1',
      name: 'test-token',
      token: 'secret-plaintext',
      expires_at: '2027-01-01T00:00:00Z',
      created_at: '2026-01-01T00:00:00Z',
    });
    const client = authedClient(fn);

    const res = await client.tokens.create({ name: 'test-token', expires_in_days: 30 });

    expect(res.id).toBe('tok_1');
    expect(res.name).toBe('test-token');
    expect(res.token).toBe('secret-plaintext');
    expect(calls[0].url).toBe(`${BASE}/customer/tokens`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ name: 'test-token', expires_in_days: 30 })
    );
    expectAuth(calls);
  });

  it('create() with empty request → POST /customer/tokens', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'tok_2',
      name: '',
      token: 'secret',
      created_at: '2026-01-01T00:00:00Z',
    });
    const client = authedClient(fn);

    const res = await client.tokens.create({});

    expect(res.id).toBe('tok_2');
    expect(calls[0].url).toBe(`${BASE}/customer/tokens`);
    expect(calls[0].init?.method).toBe('POST');
    expectAuth(calls);
  });
});
