import { describe, it, expect } from 'vitest';
import { createAdminClient } from '../admin';
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

const BASE = 'http://localhost:8090/api';
const API_KEY = 'test-api-key-123';

function makeClient(fetch: FetchFn) {
  return createAdminClient({ baseUrl: BASE, apiKey: API_KEY, fetch });
}

function headersOf(call: { init?: RequestInit }): Record<string, string> {
  return (call.init?.headers ?? {}) as Record<string, string>;
}

describe('Admin Client — tenants', () => {
  it('list() → GET /tenants', async () => {
    const resp = { items: [], total: 0, page: 1, size: 20 };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.list();
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants`);
    expect(calls[0].init?.method).toBe('GET');
  });

  it('list() with params → GET /tenants?page=1&size=10&state=ACTIVE', async () => {
    const { fn, calls } = capturingFetch(200, {
      items: [],
      total: 0,
      page: 1,
      size: 10,
    });
    await makeClient(fn).tenants.list({ page: 1, size: 10, state: 'ACTIVE' });
    expect(calls[0].url).toContain('/tenants?');
    expect(calls[0].url).toContain('page=1');
    expect(calls[0].url).toContain('size=10');
    expect(calls[0].url).toContain('state=ACTIVE');
  });

  it('create() → POST /tenants', async () => {
    const resp = {
      id: 'abc123',
      admin_user: 'admin',
      admin_password: 'pw',
      connection_string: 'psql ...',
      created_at: '2026-01-01',
    };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.create();
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants`);
    expect(calls[0].init?.method).toBe('POST');
  });

  it('get(id) → GET /tenants/tid', async () => {
    const resp = { id: 'tid', state: 'ACTIVE' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.get('tid');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid`);
    expect(calls[0].init?.method).toBe('GET');
  });

  it('update(id, req) → PUT /tenants/tid', async () => {
    const resp = { id: 'tid', state: 'ACTIVE', notes: 'test' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.update('tid', {
      notes: 'test',
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid`);
    expect(calls[0].init?.method).toBe('PUT');
    expect(calls[0].init?.body).toBe(JSON.stringify({ notes: 'test' }));
  });

  it('delete(id) → DELETE /tenants/tid', async () => {
    const resp = { message: 'deleted' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.delete('tid');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid`);
    expect(calls[0].init?.method).toBe('DELETE');
  });

  it('remove(id) → POST /tenants/tid/remove', async () => {
    const resp = { message: 'removed' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.remove('tid');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/remove`);
    expect(calls[0].init?.method).toBe('POST');
  });
});

describe('Admin Client — batch operations', () => {
  it('batchCreate → POST /tenants/batch', async () => {
    const resp = {
      created: [],
      failed: [],
      total_requested: 3,
      total_created: 3,
    };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.batchCreate({ count: 3 });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/batch`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ count: 3 }));
  });

  it('batchDelete → POST /tenants/batch/delete', async () => {
    const resp = { deleted: ['a', 'b'], failed: [] };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.batchDelete({
      ids: ['a', 'b'],
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/batch/delete`);
    expect(calls[0].init?.method).toBe('POST');
  });

  it('batchUpdate → PUT /tenants/batch', async () => {
    const resp = { updated: ['a'], failed: [] };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.batchUpdate({
      ids: ['a'],
      notes: 'x',
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/batch`);
    expect(calls[0].init?.method).toBe('PUT');
  });
});

describe('Admin Client — session operations', () => {
  it('connect → POST /tenants/tid/connect', async () => {
    const resp = { session_id: 'sess1', expires_at: '2026-01-01T01:00:00Z' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.connect('tid', {
      admin_user: 'admin',
      admin_password: 'pass',
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/connect`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ admin_user: 'admin', admin_password: 'pass' })
    );
  });

  it('query → POST /tenants/tid/query with X-Tenant-Session', async () => {
    const resp = { success: true, result: '1' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).tenants.query('tid', 'session123', {
      sql: 'SELECT 1',
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/query`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ sql: 'SELECT 1' }));
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('session123');
  });
});

describe('Admin Client — system', () => {
  it('health() → GET /health', async () => {
    const resp = { status: 'ok', pd_healthy: true };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).system.health();
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/health`);
    expect(calls[0].init?.method).toBe('GET');
  });

  it('info() → GET /info', async () => {
    const resp = {
      name: 'pg-tikv Admin API',
      version: '2.0.0',
      docs: '/docs',
    };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).system.info();
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/info`);
    expect(calls[0].init?.method).toBe('GET');
  });
});

describe('Admin Client — headers', () => {
  it('X-API-Key is present on all tenant requests', async () => {
    const { fn, calls } = capturingFetch(200, {
      items: [],
      total: 0,
      page: 1,
      size: 20,
    });
    await makeClient(fn).tenants.list();
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('X-API-Key is present on system requests', async () => {
    const { fn, calls } = capturingFetch(200, {
      status: 'ok',
      pd_healthy: true,
    });
    await makeClient(fn).system.health();
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('X-API-Key is present on query requests alongside X-Tenant-Session', async () => {
    const { fn, calls } = capturingFetch(200, { success: true });
    await makeClient(fn).tenants.query('tid', 'sess', { sql: 'SELECT 1' });
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('sess');
  });

  it('no X-API-Key when apiKey not provided', async () => {
    const { fn, calls } = capturingFetch(200, {
      items: [],
      total: 0,
      page: 1,
      size: 20,
    });
    const client = createAdminClient({ baseUrl: BASE, fetch: fn });
    await client.tenants.list();
    expect(headersOf(calls[0])['X-API-Key']).toBeUndefined();
  });
});
