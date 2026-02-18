import { describe, it, expect } from 'vitest';
import { instantDatabase } from '../index';
import { MemoryCredentialStore } from '../credentials';
import type { FetchFn } from '../http';

const BASE = 'http://test:8090/api';

function createMockFetch(responses: Map<string, unknown>) {
  const calls: { url: string; method: string; body?: unknown }[] = [];
  const fn: FetchFn = async (url, init) => {
    const method = init?.method ?? 'GET';
    const key = `${method} ${new URL(url.toString()).pathname}`;
    calls.push({
      url: url.toString(),
      method,
      body:
        typeof init?.body === 'string'
          ? (JSON.parse(init.body) as unknown)
          : undefined,
    });
    const responseBody = responses.get(key);
    return new Response(responseBody ? JSON.stringify(responseBody) : null, {
      status: responseBody ? 200 : 404,
      headers: { 'Content-Type': 'application/json' },
    });
  };
  return { fn, calls };
}

function buildDatabaseResponse(name = 'default') {
  return {
    id: 'db1',
    name,
    state: 'ACTIVE',
    admin_user: 'admin',
    admin_password: 'pass',
    created_at: '2026-01-01T00:00:00Z',
    connection_string: 'postgresql://example',
  };
}

function countCalls(
  calls: { url: string; method: string; body?: unknown }[],
  method: string,
  path: string
) {
  return calls.filter((call) => {
    const callPath = new URL(call.url).pathname;
    return call.method === method && callPath === path;
  }).length;
}

describe('instantDatabase()', () => {
  it('runs fresh flow, creates database, and saves credentials', async () => {
    const store = new MemoryCredentialStore();
    const responses = new Map<string, unknown>([
      [
        'POST /api/customer/anonymous-register',
        {
          token: 'tok',
          expires_at: '2026-12-31T00:00:00Z',
          is_anonymous: true,
          anonymous_id: 'aid',
          anonymous_secret: 'asec',
        },
      ],
      ['GET /api/customer/databases', []],
      ['POST /api/customer/databases', buildDatabaseResponse()],
    ]);
    const { fn, calls } = createMockFetch(responses);

    const result = await instantDatabase({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    expect(result).toEqual({
      databaseId: 'db1',
      connectionString: 'postgresql://example',
      adminUser: 'admin',
      adminPassword: 'pass',
      state: 'ACTIVE',
      createdAt: '2026-01-01T00:00:00Z',
    });
    expect(calls).toHaveLength(3);

    const saved = await store.load();
    expect(saved).toEqual({
      token: 'tok',
      is_anonymous: true,
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });
  });

  it('uses existing credentials and skips anonymous registration', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });
    const responses = new Map<string, unknown>([
      ['GET /api/customer/databases', []],
      ['POST /api/customer/databases', buildDatabaseResponse()],
    ]);
    const { fn, calls } = createMockFetch(responses);

    const result = await instantDatabase({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    expect(result.databaseId).toBe('db1');
    expect(countCalls(calls, 'POST', '/api/customer/anonymous-register')).toBe(0);
    expect(calls).toHaveLength(2);
  });

  it('is idempotent and returns existing database when found', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });
    const responses = new Map<string, unknown>([
      ['GET /api/customer/databases', [buildDatabaseResponse()]],
    ]);
    const { fn, calls } = createMockFetch(responses);

    const result = await instantDatabase({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    expect(result.databaseId).toBe('db1');
    expect(countCalls(calls, 'POST', '/api/customer/databases')).toBe(0);
    expect(calls).toHaveLength(1);
  });

  it('executes seed SQL when seed is provided', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });
    const responses = new Map<string, unknown>([
      ['GET /api/customer/databases', []],
      ['POST /api/customer/databases', buildDatabaseResponse()],
      [
        'POST /api/customer/databases/db1/sql',
        { columns: [], rows: [], row_count: 0, command: 'CREATE' },
      ],
    ]);
    const { fn, calls } = createMockFetch(responses);

    await instantDatabase({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
      seed: 'CREATE TABLE t(id INT)',
    });

    const seedCall = calls.find(
      (call) =>
        call.method === 'POST' &&
        new URL(call.url).pathname === '/api/customer/databases/db1/sql'
    );
    expect(seedCall?.body).toEqual({ query: 'CREATE TABLE t(id INT)' });
  });

  it('executes seed file content when seedFile is provided', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });
    const responses = new Map<string, unknown>([
      ['GET /api/customer/databases', []],
      ['POST /api/customer/databases', buildDatabaseResponse()],
      [
        'POST /api/customer/databases/db1/sql',
        { columns: [], rows: [], row_count: 0, command: 'CREATE' },
      ],
    ]);
    const { fn, calls } = createMockFetch(responses);

    await instantDatabase({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
      seedFile: 'CREATE TABLE t(id INT)',
    });

    const seedCall = calls.find(
      (call) =>
        call.method === 'POST' &&
        new URL(call.url).pathname === '/api/customer/databases/db1/sql'
    );
    expect(seedCall?.body).toEqual({ file_content: 'CREATE TABLE t(id INT)' });
  });

  it('creates database with custom name', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });
    const responses = new Map<string, unknown>([
      ['GET /api/customer/databases', []],
      ['POST /api/customer/databases', buildDatabaseResponse('myapp')],
    ]);
    const { fn, calls } = createMockFetch(responses);

    await instantDatabase({
      name: 'myapp',
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    const createCall = calls.find(
      (call) =>
        call.method === 'POST' && new URL(call.url).pathname === '/api/customer/databases'
    );
    expect(createCall?.body).toEqual({ name: 'myapp' });
  });
});
