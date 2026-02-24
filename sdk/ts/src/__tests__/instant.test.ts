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

  it('propagates timeout option to HttpClient', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });

    // Mock fetch that never resolves (simulates slow network)
    const fn: FetchFn = async (_url, init) => {
      return new Promise<Response>((_resolve, reject) => {
        if (init?.signal) {
          init.signal.addEventListener('abort', () => {
            reject(new DOMException('The operation was aborted.', 'AbortError'));
          });
        }
      });
    };

    await expect(
      instantDatabase({
        baseUrl: BASE,
        fetch: fn,
        credentialStore: store,
        timeout: 50,
      })
    ).rejects.toThrow();
  });

  it('propagates maxRetries option — retries on 5xx', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });

    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      return new Response(JSON.stringify({ message: 'Error' }), { status: 503 });
    };

    await expect(
      instantDatabase({
        baseUrl: BASE,
        fetch: fn,
        credentialStore: store,
        maxRetries: 2,
        retryDelay: 1,
      })
    ).rejects.toThrow();
    // 1 original + 2 retries = 3 calls
    expect(callCount).toBe(3);
  });

  it('propagates retryDelay option — delays between retries', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'existing-token' });

    const timestamps: number[] = [];
    const fn: FetchFn = async () => {
      timestamps.push(Date.now());
      return new Response(JSON.stringify({ message: 'Error' }), { status: 503 });
    };

    await expect(
      instantDatabase({
        baseUrl: BASE,
        fetch: fn,
        credentialStore: store,
        maxRetries: 1,
        retryDelay: 50,
      })
    ).rejects.toThrow();
    expect(timestamps).toHaveLength(2);
    // Second call should be at least 40ms after first (50ms base delay with some tolerance)
    expect(timestamps[1] - timestamps[0]).toBeGreaterThanOrEqual(40);
  });
});
