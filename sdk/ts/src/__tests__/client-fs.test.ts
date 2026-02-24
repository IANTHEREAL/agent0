import { describe, it, expect } from 'vitest';
import { createDb9Client } from '../client';
import type { FetchFn } from '../http';
import type { Fs9FileEntry } from '../fs-types';
import { MemoryCredentialStore } from '../credentials';

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

  it('sends application/octet-stream for ArrayBuffer content', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);
    const buffer = new ArrayBuffer(4);
    new Uint8Array(buffer).set([0x89, 0x50, 0x4e, 0x47]);

    await client.fs.write('db1', '/image.png', buffer);

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/upload?path=%2Fimage.png'
    );
    expect(calls[0].init?.method).toBe('PUT');
    expect(calls[0].init?.body).toBe(buffer);
    expect((calls[0].init?.headers as Record<string, string>)['Content-Type']).toBe(
      'application/octet-stream'
    );
    expectAuth(calls);
  });

  it('sends application/octet-stream for Uint8Array content', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);
    const data = new Uint8Array([0x00, 0x01, 0x02, 0x03]);

    await client.fs.write('db1', '/binary.bin', data);

    expect(calls).toHaveLength(1);
    expect(calls[0].init?.method).toBe('PUT');
    expect(calls[0].init?.body).toBe(data);
    expect((calls[0].init?.headers as Record<string, string>)['Content-Type']).toBe(
      'application/octet-stream'
    );
    expectAuth(calls);
  });
});

describe('fs.readBinary()', () => {
  it('sends GET request to /download endpoint', async () => {
    const data = new ArrayBuffer(4);
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      return new Response(data, {
        status: 200,
        headers: { 'Content-Type': 'application/octet-stream' },
      });
    };
    const client = fsClient(fn);

    await client.fs.readBinary('db1', '/image.png');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/download?path=%2Fimage.png'
    );
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns ArrayBuffer from response', async () => {
    const source = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const calls: { url: string; init?: RequestInit }[] = [];
    const fn: FetchFn = async (url, init) => {
      calls.push({ url: url.toString(), init });
      return new Response(source, {
        status: 200,
        headers: { 'Content-Type': 'application/octet-stream' },
      });
    };
    const client = fsClient(fn);

    const result = await client.fs.readBinary('db1', '/image.png');

    expect(result).toBeInstanceOf(ArrayBuffer);
    const view = new Uint8Array(result);
    expect(view[0]).toBe(0x89);
    expect(view[1]).toBe(0x50);
    expect(view[2]).toBe(0x4e);
    expect(view[3]).toBe(0x47);
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

describe('fs.exists()', () => {
  it('returns true when stat succeeds', async () => {
    const mock = mockEntry({ path: '/test.txt', size: 100, mode: 33188, mtime: 1645174800 });
    const { fn } = capturingFetch(200, mock);
    const client = fsClient(fn);

    const result = await client.fs.exists('db1', '/test.txt');

    expect(result).toBe(true);
  });

  it('returns false when stat returns 404', async () => {
    const { fn } = capturingFetch(404, { message: 'Not found' });
    const client = fsClient(fn);

    const result = await client.fs.exists('db1', '/nonexistent.txt');

    expect(result).toBe(false);
  });

  it('re-throws non-404 errors', async () => {
    const { fn } = capturingFetch(500, { message: 'Server error' });
    const client = fsClient(fn);

    await expect(client.fs.exists('db1', '/test.txt')).rejects.toThrow();
  });

  it('sends GET request to /stat endpoint', async () => {
    const mock = mockEntry({ path: '/test.txt', size: 100, mode: 33188, mtime: 1645174800 });
    const { fn, calls } = capturingFetch(200, mock);
    const client = fsClient(fn);

    await client.fs.exists('db1', '/test.txt');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toContain('/stat');
    expect(calls[0].init?.method).toBe('GET');
  });
});

describe('fs.events()', () => {
  it('sends GET request to /events endpoint with all options', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.events('db1', { limit: 50, offset: 10, path: '/uploads', type: 'write' });

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toContain('/api/v1/events');
    expect(calls[0].url).toContain('limit=50');
    expect(calls[0].url).toContain('offset=10');
    expect(calls[0].url).toContain('path=%2Fuploads');
    expect(calls[0].url).toContain('type=write');
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('sends GET to /events without query params when no options', async () => {
    const { fn, calls } = capturingFetch(200, []);
    const client = fsClient(fn);

    await client.fs.events('db1');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('http://test:8090/fs9/db1/api/v1/events');
    expect(calls[0].init?.method).toBe('GET');
    expectAuth(calls);
  });

  it('returns typed Fs9EventEntry array', async () => {
    const mockEvents = [
      { id: 'e1', type: 'write', path: '/test.txt', timestamp: '2026-01-01T00:00:00Z' },
      { id: 'e2', type: 'mkdir', path: '/newdir', timestamp: '2026-01-01T00:01:00Z' },
    ];
    const { fn, calls } = capturingFetch(200, mockEvents);
    const client = fsClient(fn);

    const result = await client.fs.events('db1');

    expect(result).toHaveLength(2);
    expect(result[0].type).toBe('write');
    expect(result[1].type).toBe('mkdir');
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
  it('sends POST to /mkdir endpoint with path and recursive params', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/testdir');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(
      'http://test:8090/fs9/db1/api/v1/mkdir?path=%2Ftestdir&recursive=true'
    );
    expect(calls[0].init?.method).toBe('POST');
    expectAuth(calls);
  });

  it('handles nested directory paths', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/a/b/c/d');

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toContain('path=%2Fa%2Fb%2Fc%2Fd');
    expect(calls[0].url).toContain('recursive=true');
    expectAuth(calls);
  });

  it('does not send a request body', async () => {
    const { fn, calls } = capturingFetch(200);
    const client = fsClient(fn);

    await client.fs.mkdir('db1', '/testdir');

    expect(calls[0].init?.body).toBeUndefined();
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
    const { fn, calls } = capturingFetch(200);
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

describe('fs – 401 auto-refresh', () => {
  it('concurrent 401s on fs.read trigger single refresh', async () => {
    let callCount = 0;
    let refreshCount = 0;
    const fn: FetchFn = async (url, init) => {
      const urlStr = url.toString();
      if (urlStr.includes('/anonymous-refresh')) {
        refreshCount++;
        return new Response(JSON.stringify({ token: 'new-token' }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      }
      callCount++;
      if (callCount <= 2) {
        return new Response(JSON.stringify({ message: 'Unauthorized' }), { status: 401 });
      }
      return new Response('file content', {
        status: 200,
        headers: { 'Content-Type': 'text/plain' },
      });
    };
    const store = new MemoryCredentialStore();
    await store.save({
      token: 'old-token',
      is_anonymous: true,
      anonymous_id: 'anon-1',
      anonymous_secret: 'secret-1',
    });
    const client = createDb9Client({ baseUrl: BASE, fetch: fn, credentialStore: store });

    const [r1, r2] = await Promise.all([
      client.fs.read('db1', '/file1.txt'),
      client.fs.read('db1', '/file2.txt'),
    ]);

    expect(r1).toBe('file content');
    expect(r2).toBe('file content');
    expect(refreshCount).toBe(1);
  });
});
