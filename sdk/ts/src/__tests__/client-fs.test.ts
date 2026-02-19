import { describe, it, expect } from 'vitest';
import { createDb9Client } from '../client';
import type { FetchFn } from '../http';
import type { Fs9FileInfo, Fs9StatResponse } from '../fs-types';

// ── Mock fetch helpers ──────────────────────────────────────────────

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

function capturingTextFetch(status: number, text: string) {
  const calls: { url: string; init?: RequestInit }[] = [];
  const fn: FetchFn = async (url, init) => {
    calls.push({ url: url.toString(), init });
    return new Response(text, {
      status,
      headers: { 'Content-Type': 'text/plain' },
    });
  };
  return { fn, calls };
}

// ── Test helpers ────────────────────────────────────────────────────

const BASE = 'http://test:8090/api';
const TOKEN = 'test-token';

function fsClient(fetch: FetchFn) {
  return createDb9Client({ baseUrl: BASE, fetch, token: TOKEN });
}

function expectAuth(calls: { url: string; init?: RequestInit }[]) {
  for (const call of calls) {
    expect(
      (call.init?.headers as Record<string, string>)['Authorization']
    ).toBe(`Bearer ${TOKEN}`);
  }
}

// ── Tests ───────────────────────────────────────────────────────────

describe('fs.list()', () => {
  it('sends GET request with correct URL and path parameter', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.list('db1', '/uploads/');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/readdir?path=%2Fuploads%2F'
    );
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('includes recursive parameter when option is set', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.list('db1', '/uploads/', { recursive: true });

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toContain('recursive=true');
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns typed Fs9FileInfo array from JSON response', async () => {
    const mockFiles: Fs9FileInfo[] = [
      {
        path: '/uploads/file1.txt',
        type: 'file',
        size: 1024,
        mode: 33188,
        mtime: '2026-02-19T10:00:00Z',
      },
      {
        path: '/uploads/subdir',
        type: 'dir',
        size: 4096,
        mode: 16877,
        mtime: '2026-02-19T09:00:00Z',
      },
    ];
    const { fn, calls } = capturingFetch(200, mockFiles);
    const client = fsClient(fn);

    const result = await client.fs.list('db1', '/uploads/');

    expect(result).toEqual(mockFiles);
    expect(result).toHaveLength(2);
    expect(result[0].type).toBe('file');
    expect(result[1].type).toBe('dir');
    expectAuth(calls);
  });

  it('handles empty directory listing', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    const result = await client.fs.list('db1', '/empty/');

    expect(result).toEqual([]);
    expect(calls).toHaveLength(1);
    expectAuth(calls);
  });
});

describe('fs.read()', () => {
  it('sends GET request to /download endpoint', async () => {
    const { fn, calls } = capturingTextFetch(200, 'file content');
    const client = fsClient(fn);

    await client.fs.read('db1', '/uploads/file.txt');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/download?path=%2Fuploads%2Ffile.txt'
    );
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns raw text content (not JSON parsed)', async () => {
    const content = 'Hello, World!\nLine 2\nLine 3';
    const { fn, calls } = capturingTextFetch(200, content);
    const client = fsClient(fn);

    const result = await client.fs.read('db1', '/uploads/file.txt');

    expect(result).toBe(content);
    expect(typeof result).toBe('string');
    expectAuth(calls);
  });

  it('handles empty file', async () => {
    const { fn, calls } = capturingTextFetch(200, '');
    const client = fsClient(fn);

    const result = await client.fs.read('db1', '/empty.txt');

    expect(result).toBe('');
    expectAuth(calls);
  });

  it('handles file with special characters', async () => {
    const content = 'Special: \n\t\r"quotes"\'single\'';
    const { fn, calls } = capturingTextFetch(200, content);
    const client = fsClient(fn);

    const result = await client.fs.read('db1', '/special.txt');

    expect(result).toBe(content);
    expectAuth(calls);
  });
});

describe('fs.write()', () => {
  it('sends PUT request with raw body (not JSON stringified)', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/uploads/file.txt', 'file content');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/upload?path=%2Fuploads%2Ffile.txt'
    );
    expect(calls[0].init?.method).toBe('PUT');
    expect(calls[0].init?.body).toBe('file content');
    expect((calls[0].init?.headers as Record<string, string>)['Content-Type']).toBe(
      'text/plain'
    );
    expectAuth(calls);
  });

  it('sends Content-Type: text/plain header', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/file.txt', 'content');

    expect((calls[0].init?.headers as Record<string, string>)['Content-Type']).toBe(
      'text/plain'
    );
  });

  it('handles empty content', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/empty.txt', '');

    expect(calls[0].init?.body).toBe('');
    expectAuth(calls);
  });

  it('handles multiline content', async () => {
    const content = 'Line 1\nLine 2\nLine 3';
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/multiline.txt', content);

    expect(calls[0].init?.body).toBe(content);
    expectAuth(calls);
  });

  it('handles special characters in content', async () => {
    const content = 'Special: \n\t\r"quotes"\'single\'';
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/special.txt', content);

    expect(calls[0].init?.body).toBe(content);
    expectAuth(calls);
  });
});

describe('fs.stat()', () => {
  it('sends GET request to /stat endpoint', async () => {
    const mockStat: Fs9StatResponse = {
      path: '/uploads/file.txt',
      is_dir: false,
      is_file: true,
      size: 1024,
      mode: 33188,
      mtime: 1645174800,
    };
    const { fn, calls } = capturingFetch(200, mockStat);
    const client = fsClient(fn);

    await client.fs.stat('db1', '/uploads/file.txt');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/stat?path=%2Fuploads%2Ffile.txt'
    );
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns typed Fs9StatResponse from JSON response', async () => {
    const mockStat: Fs9StatResponse = {
      path: '/uploads/file.txt',
      is_dir: false,
      is_file: true,
      size: 2048,
      mode: 33188,
      mtime: 1645174800,
    };
    const { fn, calls } = capturingFetch(200, mockStat);
    const client = fsClient(fn);

    const result = await client.fs.stat('db1', '/uploads/file.txt');

    expect(result).toEqual(mockStat);
    expect(result.is_file).toBe(true);
    expect(result.is_dir).toBe(false);
    expect(result.size).toBe(2048);
    expectAuth(calls);
  });

  it('returns stat for directory', async () => {
    const mockStat: Fs9StatResponse = {
      path: '/uploads',
      is_dir: true,
      is_file: false,
      size: 4096,
      mode: 16877,
      mtime: 1645174800,
    };
    const { fn, calls } = capturingFetch(200, mockStat);
    const client = fsClient(fn);

    const result = await client.fs.stat('db1', '/uploads');

    expect(result.is_dir).toBe(true);
    expect(result.is_file).toBe(false);
    expectAuth(calls);
  });
});

describe('fs – URL derivation', () => {
  it('strips /api suffix from baseUrl', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = createDb9Client({
      baseUrl: 'http://test:8090/api',
      fetch: fn,
      token: TOKEN,
    });

    await client.fs.list('db1', '/path');

    expect(calls[0].url).toContain('http://test:8090/fs9/db1/api/v1');
    expect(calls[0].url).not.toContain('/api/api');
  });

  it('strips /api/ (with trailing slash) from baseUrl', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = createDb9Client({
      baseUrl: 'http://test:8090/api/',
      fetch: fn,
      token: TOKEN,
    });

    await client.fs.list('db1', '/path');

    expect(calls[0].url).toContain('http://test:8090/fs9/db1/api/v1');
    expect(calls[0].url).not.toContain('/api/api');
  });

  it('handles baseUrl without /api suffix', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = createDb9Client({
      baseUrl: 'http://test:8090',
      fetch: fn,
      token: TOKEN,
    });

    await client.fs.list('db1', '/path');

    expect(calls[0].url).toContain('http://test:8090/fs9/db1/api/v1');
  });

  it('handles HTTPS URLs', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = createDb9Client({
      baseUrl: 'https://db9.example.com/api',
      fetch: fn,
      token: TOKEN,
    });

    await client.fs.list('db1', '/path');

    expect(calls[0].url).toContain('https://db9.example.com/fs9/db1/api/v1');
  });

  it('correctly encodes path parameters', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.list('db1', '/uploads/my file.txt');

    expect(calls[0].url).toContain('path=%2Fuploads%2Fmy+file.txt');
  });
});

describe('fs – Error handling', () => {
  it('throws Db9Error on 404 status', async () => {
    const { fn } = capturingFetch(404, { error: 'Not found' });
    const client = fsClient(fn);

    await expect(client.fs.list('db1', '/nonexistent')).rejects.toThrow();
  });

  it('throws Db9Error on 500 status', async () => {
    const { fn } = capturingFetch(500, { error: 'Internal server error' });
    const client = fsClient(fn);

    await expect(client.fs.read('db1', '/file.txt')).rejects.toThrow();
  });

  it('throws Db9Error on 403 status', async () => {
    const { fn } = capturingFetch(403, { error: 'Forbidden' });
    const client = fsClient(fn);

    await expect(client.fs.write('db1', '/file.txt', 'content')).rejects.toThrow();
  });

  it('throws Db9Error on 400 status', async () => {
    const { fn } = capturingFetch(400, { error: 'Bad request' });
    const client = fsClient(fn);

    await expect(client.fs.stat('db1', '/file.txt')).rejects.toThrow();
  });
});

describe('fs – Authentication', () => {
  it('includes Bearer token in Authorization header for list', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.list('db1', '/path');

    expectAuth(calls);
  });

  it('includes Bearer token in Authorization header for read', async () => {
    const { fn, calls } = capturingTextFetch(200, 'content');
    const client = fsClient(fn);

    await client.fs.read('db1', '/path');

    expectAuth(calls);
  });

  it('includes Bearer token in Authorization header for write', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.write('db1', '/path', 'content');

    expectAuth(calls);
  });

  it('includes Bearer token in Authorization header for stat', async () => {
    const { fn, calls } = capturingFetch(200, {
      path: '/path',
      is_dir: false,
      is_file: true,
      size: 100,
      mode: 33188,
      mtime: 1645174800,
    });
    const client = fsClient(fn);

    await client.fs.stat('db1', '/path');

    expectAuth(calls);
  });
});
