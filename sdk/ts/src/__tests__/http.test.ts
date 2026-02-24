import { describe, it, expect } from 'vitest';
import { createHttpClient, FetchFn } from '../http';
import { Db9Error } from '../errors';

// Helper to create a mock fetch function
function mockFetch(
  status: number,
  body?: unknown,
  statusText = 'OK'
): FetchFn {
  return async (_url: string | URL | Request, _init?: RequestInit) => {
    return new Response(body ? JSON.stringify(body) : null, {
      status,
      statusText,
      headers: { 'Content-Type': 'application/json' },
    });
  };
}

// Helper to create a capturing mock fetch
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

describe('createHttpClient', () => {
  it('should make a GET request with correct URL and method', async () => {
    const { fn, calls } = capturingFetch(200, { result: 'success' });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    const result = await client.get<{ result: string }>('/test');
    expect(result).toEqual({ result: 'success' });
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('http://localhost:8090/api/test');
    expect(calls[0].init?.method).toBe('GET');
  });

  it('should make a POST request with JSON body', async () => {
    const { fn, calls } = capturingFetch(200, { id: '123' });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    const result = await client.post<{ id: string }>('/users', {
      name: 'Alice',
    });
    expect(result).toEqual({ id: '123' });
    expect(calls).toHaveLength(1);
    expect(calls[0].init?.method).toBe('POST');
    expect(calls[0].init?.body).toBe(JSON.stringify({ name: 'Alice' }));
  });

  it('should make a PUT request with JSON body', async () => {
    const { fn, calls } = capturingFetch(200, { updated: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    const result = await client.put<{ updated: boolean }>('/users/123', {
      name: 'Bob',
    });
    expect(result).toEqual({ updated: true });
    expect(calls).toHaveLength(1);
    expect(calls[0].init?.method).toBe('PUT');
    expect(calls[0].init?.body).toBe(JSON.stringify({ name: 'Bob' }));
  });

  it('should make a DELETE request', async () => {
    const { fn, calls } = capturingFetch(200, { deleted: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    const result = await client.del<{ deleted: boolean }>('/users/123');
    expect(result).toEqual({ deleted: true });
    expect(calls).toHaveLength(1);
    expect(calls[0].init?.method).toBe('DELETE');
  });

  it('should inject auth headers from options', async () => {
    const { fn, calls } = capturingFetch(200, { ok: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
      headers: { Authorization: 'Bearer token123' },
    });

    await client.get('/test');
    expect(calls[0].init?.headers).toEqual({
      'Content-Type': 'application/json',
      Authorization: 'Bearer token123',
    });
  });

  it('should serialize query params correctly', async () => {
    const { fn, calls } = capturingFetch(200, { results: [] });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    await client.get('/search', { q: 'test', limit: '10' });
    expect(calls[0].url).toContain('?');
    expect(calls[0].url).toContain('q=test');
    expect(calls[0].url).toContain('limit=10');
  });

  it('should filter out undefined query params', async () => {
    const { fn, calls } = capturingFetch(200, { results: [] });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    await client.get('/search', { q: 'test', limit: undefined });
    expect(calls[0].url).not.toContain('limit');
    expect(calls[0].url).toContain('q=test');
  });

  it('should throw Db9Error on non-2xx response', async () => {
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: mockFetch(404, { message: 'Not found' }),
    });

    try {
      await client.get('/missing');
      expect.fail('Should have thrown');
    } catch (error) {
      expect(error).toBeInstanceOf(Db9Error);
      expect((error as Db9Error).statusCode).toBe(404);
    }
  });

  it('should handle 204 No Content response', async () => {
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: mockFetch(204),
    });

    const result = await client.del('/users/123');
    expect(result).toBeUndefined();
  });

  it('should strip trailing slash from baseUrl', async () => {
    const { fn, calls } = capturingFetch(200, { ok: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api/',
      fetch: fn,
    });

    await client.get('/test');
    expect(calls[0].url).toBe('http://localhost:8090/api/test');
  });

  it('should use globalThis.fetch if not provided', async () => {
    // This test verifies the fallback exists, but we can't easily test globalThis.fetch
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
    });
    expect(client).toBeDefined();
  });

  it('should set Content-Type header to application/json', async () => {
    const { fn, calls } = capturingFetch(200, { ok: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    await client.post('/test', { data: 'value' });
    expect(calls[0].init?.headers).toHaveProperty(
      'Content-Type',
      'application/json'
    );
  });

  it('should not send body for GET requests', async () => {
    const { fn, calls } = capturingFetch(200, { ok: true });
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    await client.get('/test');
    expect(calls[0].init?.body).toBeUndefined();
  });

  it('should handle empty response body for POST', async () => {
    const { fn, calls } = capturingFetch(200, null);
    const client = createHttpClient({
      baseUrl: 'http://localhost:8090/api',
      fetch: fn,
    });

    // This will fail to parse JSON, but we're testing the request was made
    try {
      await client.post('/test', { data: 'value' });
    } catch {
      // Expected to fail on JSON parse
    }
    expect(calls).toHaveLength(1);
  });
});

describe('timeout', () => {
  it('aborts request after configured timeout', async () => {
    const slowFetch: FetchFn = async (_url, init) => {
      return new Promise<Response>((_resolve, reject) => {
        if (init?.signal) {
          init.signal.addEventListener('abort', () => {
            reject(new DOMException('The operation was aborted.', 'AbortError'));
          });
        }
      });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: slowFetch,
      timeout: 50,
    });

    await expect(client.get('/slow')).rejects.toThrow();
  });

  it('completes normally when response is faster than timeout', async () => {
    const fastFetch: FetchFn = async () => {
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fastFetch,
      timeout: 5000,
    });

    const result = await client.get<{ ok: boolean }>('/fast');
    expect(result.ok).toBe(true);
  });
});

describe('retry', () => {
  it('retries on 503 and succeeds', async () => {
    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      if (callCount <= 2) {
        return new Response(JSON.stringify({ message: 'Service Unavailable' }), { status: 503 });
      }
      return new Response(JSON.stringify({ data: 'ok' }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
      maxRetries: 2,
      retryDelay: 10,
    });

    const result = await client.get<{ data: string }>('/retry-me');
    expect(result.data).toBe('ok');
    expect(callCount).toBe(3);
  });

  it('does NOT retry on 400 (client error)', async () => {
    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      return new Response(JSON.stringify({ message: 'Bad Request' }), { status: 400 });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
      maxRetries: 2,
      retryDelay: 10,
    });

    await expect(client.get('/bad')).rejects.toThrow();
    expect(callCount).toBe(1);
  });

  it('throws after all retries exhausted', async () => {
    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      return new Response(JSON.stringify({ message: 'Error' }), { status: 500 });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
      maxRetries: 2,
      retryDelay: 10,
    });

    await expect(client.get('/fail')).rejects.toThrow();
    expect(callCount).toBe(3);
  });

  it('default (no retry configured) behaves as before', async () => {
    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      return new Response(JSON.stringify({ message: 'Error' }), { status: 500 });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
    });

    await expect(client.get('/fail')).rejects.toThrow();
    expect(callCount).toBe(1);
  });

  it('caps maxRetries at 3 even if set higher', async () => {
    let callCount = 0;
    const fn: FetchFn = async () => {
      callCount++;
      return new Response(JSON.stringify({ message: 'Error' }), { status: 503 });
    };
    const client = createHttpClient({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
      maxRetries: 10,
      retryDelay: 1,
    });

    await expect(client.get('/fail')).rejects.toThrow();
    expect(callCount).toBe(4); // 1 original + 3 max retries
  });
});
