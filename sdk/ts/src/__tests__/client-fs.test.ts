import { describe, it, expect } from 'vitest';
import { createDb9Client } from '../client';
import type { FetchFn } from '../http';
import type { Fs9FileEntry } from '../fs-types';

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

function mockEntry(overrides: Partial<Fs9FileEntry> & { path: string }): Fs9FileEntry {
  return {
    size: 0,
    file_type: 'regular',
    mode: 420,
    uid: 0,
    gid: 0,
    atime: 0,
    mtime: 0,
    ctime: 0,
    etag: '',
    ...overrides,
  };
}

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

  it('returns typed Fs9FileEntry array from JSON response', async () => {
    const mockFiles: Fs9FileEntry[] = [
      mockEntry({ path: '/uploads/file1.txt', size: 1024, file_type: 'regular', mode: 33188, mtime: 1771511901 }),
      mockEntry({ path: '/uploads/subdir', size: 4096, file_type: 'directory', mode: 16877, mtime: 1771511800 }),
    ];
    const { fn, calls } = capturingFetch(200, mockFiles);
    const client = fsClient(fn);

    const result = await client.fs.list('db1', '/uploads/');

    expect(result).toEqual(mockFiles);
    expect(result).toHaveLength(2);
    expect(result[0].file_type).toBe('regular');
    expect(result[1].file_type).toBe('directory');
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
    const mock = mockEntry({ path: '/uploads/file.txt', size: 1024, mode: 33188, mtime: 1645174800 });
    const { fn, calls } = capturingFetch(200, mock);
    const client = fsClient(fn);

    await client.fs.stat('db1', '/uploads/file.txt');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/stat?path=%2Fuploads%2Ffile.txt'
    );
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns typed Fs9FileEntry from JSON response', async () => {
    const mock = mockEntry({ path: '/uploads/file.txt', size: 2048, mode: 33188, mtime: 1645174800 });
    const { fn, calls } = capturingFetch(200, mock);
    const client = fsClient(fn);

    const result = await client.fs.stat('db1', '/uploads/file.txt');

    expect(result).toEqual(mock);
    expect(result.file_type).toBe('regular');
    expect(result.size).toBe(2048);
    expectAuth(calls);
  });

  it('returns stat for directory', async () => {
    const mock = mockEntry({ path: '/uploads', file_type: 'directory', size: 4096, mode: 16877, mtime: 1645174800 });
    const { fn, calls } = capturingFetch(200, mock);
    const client = fsClient(fn);

    const result = await client.fs.stat('db1', '/uploads');

    expect(result.file_type).toBe('directory');
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

describe('fs.mkdir()', () => {
  it('sends POST to /open with create+directory flags, then POST to /close', async () => {
    let callCount = 0;
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      callCount++;
      if (callCount === 1) {
        // First call: /open
        return new Response(JSON.stringify({ handle_id: '42', metadata: {} }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      } else {
        // Second call: /close
        return new Response(null, {
          status: 204,
          headers: { 'Content-Type': 'application/json' },
        });
      }
    };
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/testdir');

    expect(calls).toHaveLength(2);
    expect(calls[0].url).toContain('/open');
    expect(calls[1].url).toContain('/close');
    expectAuth(calls);
  });

  it('includes correct JSON body with path and flags in /open request', async () => {
    let callCount = 0;
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      callCount++;
      if (callCount === 1) {
        return new Response(JSON.stringify({ handle_id: '42' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      } else {
        return new Response(null, { status: 204 });
      }
    };
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/testdir');

    const openCall = calls[0];
    expect(openCall.init?.body).toBe(
      JSON.stringify({
        path: '/testdir',
        flags: { create: true, directory: true },
      })
    );
    expect((openCall.init?.headers as Record<string, string>)['Content-Type']).toBe(
      'application/json'
    );
  });

  it('closes the handle after opening', async () => {
    let callCount = 0;
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      callCount++;
      if (callCount === 1) {
        return new Response(JSON.stringify({ handle_id: '99' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      } else {
        return new Response(null, { status: 204 });
      }
    };
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/testdir');

    const closeCall = calls[1];
    expect(closeCall.init?.body).toBe(JSON.stringify({ handle_id: '99' }));
    expect((closeCall.init?.headers as Record<string, string>)['Content-Type']).toBe(
      'application/json'
    );
  });
});

describe('fs.remove()', () => {
  it('sends DELETE request to /remove endpoint with path parameter', async () => {
    const { fn, calls } = capturingFetch(204);
    const client = fsClient(fn);

    await client.fs.remove('db1', '/testfile.txt');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toContain('/remove?path=%2Ftestfile.txt');
    expect(calls[0].init?.method).toBe('DELETE');
    expectAuth(calls);
  });

  it('works for files', async () => {
    const { fn, calls } = capturingFetch(204);
    const client = fsClient(fn);

    await client.fs.remove('db1', '/myfile.txt');

    expect(calls[0].url).toContain('/remove?path=%2Fmyfile.txt');
    expect(calls[0].init?.method).toBe('DELETE');
  });

  it('works for directories', async () => {
    const { fn, calls } = capturingFetch(204);
    const client = fsClient(fn);

    await client.fs.remove('db1', '/mydir');

    expect(calls[0].url).toContain('/remove?path=%2Fmydir');
    expect(calls[0].init?.method).toBe('DELETE');
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
    const mock = mockEntry({ path: '/path', size: 100, mode: 33188, mtime: 1645174800 });
    const { fn, calls } = capturingFetch(200, mock);
    const client = fsClient(fn);

    await client.fs.stat('db1', '/path');

    expectAuth(calls);
  });

  it('includes Bearer token in Authorization header for mkdir', async () => {
    let callCount = 0;
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      callCount++;
      if (callCount === 1) {
        return new Response(JSON.stringify({ handle_id: '42' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      } else {
        return new Response(null, { status: 204 });
      }
    };
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/path');

    expectAuth(calls);
  });

  it('includes Bearer token in Authorization header for remove', async () => {
    const { fn, calls } = capturingFetch(204);
    const client = fsClient(fn);

    await client.fs.remove('db1', '/path');

    expectAuth(calls);
  });
});
