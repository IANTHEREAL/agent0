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

// ── Users ──────────────────────────────────────────────────────

describe('Admin Client — users', () => {
  it('users.list() → GET /tenants/tid/users with X-Tenant-Session', async () => {
    const resp = [{ name: 'admin', is_superuser: true, can_login: true, can_create_db: false, can_create_role: false }];
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).users.list('tid', 'sess1');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/users`);
    expect(calls[0].init?.method).toBe('GET');
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('sess1');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('users.create() → POST /tenants/tid/users with session header', async () => {
    const resp = { username: 'app', password: 'generated', connection: 'psql ...' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).users.create('tid', 'sess1', { username: 'app' });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/users`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ username: 'app' }));
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('sess1');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('users.delete() → DELETE /tenants/tid/users/app with session header', async () => {
    const resp = { message: "User 'app' deleted" };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).users.delete('tid', 'sess1', 'app');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/users/app`);
    expect(calls[0].init?.method).toBe('DELETE');
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('sess1');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('users.resetPassword() → POST /tenants/tid/users/app/password with session header', async () => {
    const resp = { username: 'app', password: 'newpass' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).users.resetPassword('tid', 'sess1', 'app');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/users/app/password`);
    expect(calls[0].init?.method).toBe('POST');
    expect(headersOf(calls[0])['X-Tenant-Session']).toBe('sess1');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });
});

// ── Observability ──────────────────────────────────────────────

describe('Admin Client — observability', () => {
  it('observability.get() → GET /tenants/tid/observability', async () => {
    const resp = {
      summary: {
        window_seconds: 60, statement_count: 10, txn_commit_count: 5,
        error_count: 0, qps: 1.5, tps: 0.8, latency_avg_ms: 2.1,
        latency_p99_ms: 10, active_connections: 3,
      },
      samples: [],
    };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).observability.get('tid');
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/observability`);
    expect(calls[0].init?.method).toBe('GET');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('observability.bootstrap() → POST /tenants/tid/observability/bootstrap', async () => {
    const resp = { message: 'Observer bootstrapped' };
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).observability.bootstrap('tid', {
      admin_user: 'admin',
      admin_password: 'pass',
    });
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/tenants/tid/observability/bootstrap`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ admin_user: 'admin', admin_password: 'pass' })
    );
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });
});

// ── Audit ──────────────────────────────────────────────────────

describe('Admin Client — audit', () => {
  it('audit.list() → GET /audit-logs (no params)', async () => {
    const resp: unknown[] = [];
    const { fn, calls } = capturingFetch(200, resp);
    const result = await makeClient(fn).audit.list();
    expect(result).toEqual(resp);
    expect(calls[0].url).toBe(`${BASE}/audit-logs`);
    expect(calls[0].init?.method).toBe('GET');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('audit.list() with params → GET /audit-logs?tenant_id=tid&limit=10', async () => {
    const resp: unknown[] = [];
    const { fn, calls } = capturingFetch(200, resp);
    await makeClient(fn).audit.list({ tenant_id: 'tid', limit: 10 });
    expect(calls[0].url).toContain('/audit-logs?');
    expect(calls[0].url).toContain('tenant_id=tid');
    expect(calls[0].url).toContain('limit=10');
    expect(headersOf(calls[0])['X-API-Key']).toBe(API_KEY);
  });

  it('audit.list() with all params', async () => {
    const { fn, calls } = capturingFetch(200, []);
    await makeClient(fn).audit.list({
      tenant_id: 'tid',
      operation_type: 'CREATE',
      resource_type: 'USER',
      success: true,
      limit: 50,
      offset: 10,
    });
    const url = calls[0].url;
    expect(url).toContain('tenant_id=tid');
    expect(url).toContain('operation_type=CREATE');
    expect(url).toContain('resource_type=USER');
    expect(url).toContain('success=true');
    expect(url).toContain('limit=50');
    expect(url).toContain('offset=10');
  });
});
