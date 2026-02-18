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

describe('databases – CRUD', () => {
  it('create() → POST /customer/databases with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'db1',
      name: 'mydb',
      state: 'ACTIVE',
      created_at: '2026-01-01T00:00:00Z',
    });
    const client = authedClient(fn);

    const res = await client.databases.create({ name: 'mydb' });

    expect(res.id).toBe('db1');
    expect(res.name).toBe('mydb');
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(`${BASE}/customer/databases`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ name: 'mydb' }));
    expectAuth(calls);
  });

  it('list() → GET /customer/databases', async () => {
    const { fn, calls } = capturingFetch(200, [
      { id: 'db1', name: 'mydb', state: 'ACTIVE', created_at: '2026-01-01T00:00:00Z' },
    ]);
    const client = authedClient(fn);

    const res = await client.databases.list();

    expect(res).toHaveLength(1);
    expect(res[0].id).toBe('db1');
    expect(calls[0].url).toBe(`${BASE}/customer/databases`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('get() → GET /customer/databases/:id', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'db1',
      name: 'mydb',
      state: 'ACTIVE',
      created_at: '2026-01-01T00:00:00Z',
    });
    const client = authedClient(fn);

    const res = await client.databases.get('db1');

    expect(res.id).toBe('db1');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('delete() → DELETE /customer/databases/:id', async () => {
    const { fn, calls } = capturingFetch(200, { message: 'Database deleted' });
    const client = authedClient(fn);

    const res = await client.databases.delete('db1');

    expect(res.message).toBe('Database deleted');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1`);
    expect(calls[0].init?.method).toBe('DELETE');
    expectAuth(calls);
  });

  it('resetPassword() → POST /customer/databases/:id/reset-password', async () => {
    const { fn, calls } = capturingFetch(200, {
      admin_user: 'admin',
      admin_password: 'newpass',
      connection_string: 'psql -h ...',
    });
    const client = authedClient(fn);

    const res = await client.databases.resetPassword('db1');

    expect(res.admin_password).toBe('newpass');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/reset-password`);
    expect(calls[0].init?.method).toBe('POST');
    expectAuth(calls);
  });

  it('observability() → GET /customer/databases/:id/observability', async () => {
    const { fn, calls } = capturingFetch(200, {
      summary: {
        window_seconds: 60,
        statement_count: 10,
        txn_commit_count: 5,
        error_count: 0,
        qps: 1.5,
        tps: 0.8,
        latency_avg_ms: 12,
        latency_p99_ms: 50,
        active_connections: 3,
      },
      samples: [],
    });
    const client = authedClient(fn);

    const res = await client.databases.observability('db1');

    expect(res.summary.qps).toBe(1.5);
    expect(res.samples).toEqual([]);
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/observability`);
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });
});

describe('databases – SQL execution', () => {
  it('sql() → POST /customer/databases/:id/sql with { query }', async () => {
    const { fn, calls } = capturingFetch(200, {
      columns: [{ name: '?column?', type: 'integer' }],
      rows: [[1]],
      row_count: 1,
      command: 'SELECT',
    });
    const client = authedClient(fn);

    const res = await client.databases.sql('db1', 'SELECT 1');

    expect(res.row_count).toBe(1);
    expect(res.command).toBe('SELECT');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/sql`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ query: 'SELECT 1' }));
    expectAuth(calls);
  });

  it('sqlFile() → POST /customer/databases/:id/sql with { file_content }', async () => {
    const fileContent = 'CREATE TABLE t(id INT)';
    const { fn, calls } = capturingFetch(200, {
      columns: [],
      rows: [],
      row_count: 0,
      command: 'CREATE',
    });
    const client = authedClient(fn);

    const res = await client.databases.sqlFile('db1', fileContent);

    expect(res.command).toBe('CREATE');
    expect(calls[0].url).toBe(`${BASE}/customer/databases/db1/sql`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ file_content: fileContent })
    );
    expectAuth(calls);
  });
});

describe('databases – all methods require Authorization', () => {
  it('every database method sends Bearer token', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'db1',
      name: 'mydb',
      state: 'ACTIVE',
      created_at: '2026-01-01T00:00:00Z',
      message: 'ok',
      admin_user: 'admin',
      admin_password: 'p',
      connection_string: 's',
      summary: { window_seconds: 0, statement_count: 0, txn_commit_count: 0, error_count: 0, qps: 0, tps: 0, latency_avg_ms: 0, latency_p99_ms: 0, active_connections: 0 },
      samples: [],
      columns: [],
      rows: [],
      row_count: 0,
      command: 'SELECT',
    });
    const client = authedClient(fn);

    await client.databases.create({ name: 'mydb' });
    await client.databases.list();
    await client.databases.get('db1');
    await client.databases.delete('db1');
    await client.databases.resetPassword('db1');
    await client.databases.observability('db1');
    await client.databases.sql('db1', 'SELECT 1');
    await client.databases.sqlFile('db1', 'SELECT 1');

    expect(calls).toHaveLength(8);
    expectAuth(calls);
  });
});
