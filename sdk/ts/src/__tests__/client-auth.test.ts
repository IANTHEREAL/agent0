import { describe, it, expect } from 'vitest';
import { createDb9Client } from '../client';
import { MemoryCredentialStore } from '../credentials';
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

describe('createDb9Client – auth endpoints', () => {
  it('register() → POST /customer/register with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({ baseUrl: BASE, fetch: fn });

    const res = await client.auth.register({
      email: 'a@b.com',
      password: 'pw',
    });

    expect(res.id).toBe('c1');
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(`${BASE}/customer/register`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ email: 'a@b.com', password: 'pw' })
    );
  });

  it('login() → POST /customer/login with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      token: 'tok123',
      expires_at: '2026-12-31T00:00:00Z',
    });
    const client = createDb9Client({ baseUrl: BASE, fetch: fn });

    const res = await client.auth.login({
      email: 'a@b.com',
      password: 'pw',
    });

    expect(res.token).toBe('tok123');
    expect(calls[0].url).toBe(`${BASE}/customer/login`);
    expect(calls[0].init?.method).toBe('POST');
  });

  it('anonymousRegister() → POST /customer/anonymous-register (no body)', async () => {
    const { fn, calls } = capturingFetch(200, {
      token: 'anon-tok',
      expires_at: '2026-12-31T00:00:00Z',
      is_anonymous: true,
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });
    const client = createDb9Client({ baseUrl: BASE, fetch: fn });

    const res = await client.auth.anonymousRegister();

    expect(res.is_anonymous).toBe(true);
    expect(calls[0].url).toBe(`${BASE}/customer/anonymous-register`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBeUndefined();
  });

  it('anonymousRefresh() → POST /customer/anonymous-refresh with body', async () => {
    const { fn, calls } = capturingFetch(200, {
      token: 'refreshed-tok',
      expires_at: '2026-12-31T00:00:00Z',
    });
    const client = createDb9Client({ baseUrl: BASE, fetch: fn });

    const res = await client.auth.anonymousRefresh({
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });

    expect(res.token).toBe('refreshed-tok');
    expect(calls[0].url).toBe(`${BASE}/customer/anonymous-refresh`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ anonymous_id: 'aid', anonymous_secret: 'asec' })
    );
  });

  it('me() → GET /customer/me with Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    const res = await client.auth.me();

    expect(res.email).toBe('a@b.com');
    expect(calls[0].url).toBe(`${BASE}/customer/me`);
    expect(calls[0].init?.method).toBe('GET');
    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer my-token');
  });

  it('getAnonymousSecret() → GET /customer/anonymous-secret with Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, {
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    const res = await client.auth.getAnonymousSecret();

    expect(res.anonymous_id).toBe('aid');
    expect(calls[0].url).toBe(`${BASE}/customer/anonymous-secret`);
    expect(calls[0].init?.method).toBe('GET');
    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer my-token');
  });

  it('claim() → POST /customer/claim with body and Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      claimed: true,
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    const res = await client.auth.claim({
      email: 'a@b.com',
      password: 'pw',
    });

    expect(res.claimed).toBe(true);
    expect(calls[0].url).toBe(`${BASE}/customer/claim`);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(
      JSON.stringify({ email: 'a@b.com', password: 'pw' })
    );
    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer my-token');
  });
});

describe('createDb9Client – token endpoints', () => {
  it('tokens.list() → GET /customer/tokens with Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, [
      { id: 't1', name: 'default', created_at: '2026-01-01T00:00:00Z' },
    ]);
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    const res = await client.tokens.list();

    expect(res).toHaveLength(1);
    expect(res[0].id).toBe('t1');
    expect(calls[0].url).toBe(`${BASE}/customer/tokens`);
    expect(calls[0].init?.method).toBe('GET');
    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer my-token');
  });

  it('tokens.revoke() → DELETE /customer/tokens/:id with Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, { message: 'Token revoked' });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    const res = await client.tokens.revoke('t1');

    expect(res.message).toBe('Token revoked');
    expect(calls[0].url).toBe(`${BASE}/customer/tokens/t1`);
    expect(calls[0].init?.method).toBe('DELETE');
    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer my-token');
  });
});

describe('createDb9Client – auth contract', () => {
  it('public endpoints do NOT send Authorization header', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'my-token',
    });

    await client.auth.register({ email: 'a@b.com', password: 'pw' });
    await client.auth.login({ email: 'a@b.com', password: 'pw' });
    await client.auth.anonymousRegister();
    await client.auth.anonymousRefresh({
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });

    for (const call of calls) {
      const headers = call.init?.headers as Record<string, string>;
      expect(headers['Authorization']).toBeUndefined();
    }
  });

  it('authenticated endpoints DO send Authorization: Bearer <token>', async () => {
    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'secret-tok',
    });

    await client.auth.me();
    await client.auth.getAnonymousSecret();
    await client.auth.claim({ email: 'a@b.com', password: 'pw' });
    await client.tokens.list();
    await client.tokens.revoke('t1');

    for (const call of calls) {
      const headers = call.init?.headers as Record<string, string>;
      expect(headers['Authorization']).toBe('Bearer secret-tok');
    }
  });

  it('auto-registers anonymously when no token available', async () => {
    const store = new MemoryCredentialStore();
    const { fn, calls } = capturingFetch(200, {
      token: 'anon-tok',
      expires_at: '2026-12-31T00:00:00Z',
      is_anonymous: true,
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    await client.auth.me();

    expect(calls[0].url).toBe(`${BASE}/customer/anonymous-register`);
    expect(calls[1].url).toBe(`${BASE}/customer/me`);
    const saved = await store.load();
    expect(saved).toEqual({
      token: 'anon-tok',
      is_anonymous: true,
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
    });
  });
});

describe('createDb9Client – CredentialStore auto-loading', () => {
  it('loads token from CredentialStore when no token provided', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'store-tok' });

    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    await client.auth.me();

    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer store-tok');
  });

  it('prefers explicit token over CredentialStore', async () => {
    const store = new MemoryCredentialStore();
    await store.save({ token: 'store-tok' });

    const { fn, calls } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      token: 'explicit-tok',
      credentialStore: store,
    });

    await client.auth.me();

    expect(
      (calls[0].init?.headers as Record<string, string>)['Authorization']
    ).toBe('Bearer explicit-tok');
  });

  it('auto-registers when CredentialStore is empty and no token', async () => {
    const store = new MemoryCredentialStore();
    const { fn, calls } = capturingFetch(200, {
      token: 'anon-tok',
      expires_at: '2026-12-31T00:00:00Z',
      is_anonymous: true,
      anonymous_id: 'aid',
      anonymous_secret: 'asec',
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    await client.auth.me();

    expect(calls[0].url).toBe(`${BASE}/customer/anonymous-register`);
    const saved = await store.load();
    expect(saved?.token).toBe('anon-tok');
  });

  it('loads from CredentialStore only once (caches result)', async () => {
    let loadCount = 0;
    const store: InstanceType<typeof MemoryCredentialStore> =
      new MemoryCredentialStore();
    await store.save({ token: 'store-tok' });
    const origLoad = store.load.bind(store);
    store.load = async () => {
      loadCount++;
      return origLoad();
    };

    const { fn } = capturingFetch(200, {
      id: 'c1',
      email: 'a@b.com',
      created_at: '2026-01-01T00:00:00Z',
      status: 'active',
    });
    const client = createDb9Client({
      baseUrl: BASE,
      fetch: fn,
      credentialStore: store,
    });

    await client.auth.me();
    await client.auth.me();

    expect(loadCount).toBe(1);
  });
});

describe('createDb9Client – defaults', () => {
  it('uses default baseUrl when not provided', () => {
    const client = createDb9Client();
    expect(client).toBeDefined();
    expect(client.auth).toBeDefined();
    expect(client.tokens).toBeDefined();
  });
});
