import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import * as os from 'node:os';
import * as path from 'node:path';
import * as fs from 'node:fs/promises';
import {
  MemoryCredentialStore,
  FileCredentialStore,
  defaultCredentialStore,
  type Credentials,
} from '../credentials';

// ---------------------------------------------------------------------------
// MemoryCredentialStore
// ---------------------------------------------------------------------------

describe('MemoryCredentialStore', () => {
  let store: MemoryCredentialStore;

  beforeEach(() => {
    store = new MemoryCredentialStore();
  });

  it('returns null when no credentials saved', async () => {
    expect(await store.load()).toBeNull();
  });

  it('saves and loads token-only credentials', async () => {
    await store.save({ token: 'test-token-123' });
    const loaded = await store.load();
    expect(loaded).toEqual({ token: 'test-token-123' });
  });

  it('saves and loads credentials with all fields', async () => {
    const creds: Credentials = {
      token: 'full-token',
      is_anonymous: true,
      anonymous_id: 'anon-123',
      anonymous_secret: 'secret-456',
    };
    await store.save(creds);

    const loaded = await store.load();
    expect(loaded?.token).toBe('full-token');
    expect(loaded?.is_anonymous).toBe(true);
    expect(loaded?.anonymous_id).toBe('anon-123');
    expect(loaded?.anonymous_secret).toBe('secret-456');
  });

  it('merges credentials on subsequent saves', async () => {
    // Save token + anonymous fields
    await store.save({
      token: 'tok-1',
      is_anonymous: true,
      anonymous_id: 'id-1',
      anonymous_secret: 'sec-1',
    });

    // Save with only token — anonymous fields must be preserved
    await store.save({ token: 'tok-2' });

    const loaded = await store.load();
    expect(loaded?.token).toBe('tok-2');
    expect(loaded?.is_anonymous).toBe(true);
    expect(loaded?.anonymous_id).toBe('id-1');
    expect(loaded?.anonymous_secret).toBe('sec-1');
  });

  it('clears credentials', async () => {
    await store.save({ token: 'to-be-cleared' });
    await store.clear();
    expect(await store.load()).toBeNull();
  });

  it('load returns a copy, not the internal reference', async () => {
    await store.save({ token: 'original' });
    const a = await store.load();
    const b = await store.load();
    expect(a).toEqual(b);
    expect(a).not.toBe(b); // different object references
  });
});

// ---------------------------------------------------------------------------
// FileCredentialStore
// ---------------------------------------------------------------------------

describe('FileCredentialStore', () => {
  let tmpDir: string;
  let credFile: string;
  let store: FileCredentialStore;

  beforeEach(async () => {
    tmpDir = path.join(
      os.tmpdir(),
      `db9-test-${Date.now()}-${Math.random().toString(36).slice(2)}`,
    );
    await fs.mkdir(tmpDir, { recursive: true });
    credFile = path.join(tmpDir, 'credentials');
    store = new FileCredentialStore(credFile);
  });

  afterEach(async () => {
    try {
      await fs.rm(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore cleanup errors
    }
  });

  it('returns null for non-existent file', async () => {
    const missing = new FileCredentialStore(
      path.join(tmpDir, 'no', 'such', 'credentials'),
    );
    expect(await missing.load()).toBeNull();
  });

  it('saves and loads credentials via TOML', async () => {
    await store.save({ token: 'my-jwt-token' });
    const loaded = await store.load();
    expect(loaded).toEqual({ token: 'my-jwt-token' });
  });

  it('writes valid TOML with exact CLI field names', async () => {
    await store.save({
      token: 'tok-123',
      is_anonymous: true,
      anonymous_id: 'anon-id',
      anonymous_secret: 'anon-sec',
    });

    const raw = await fs.readFile(credFile, 'utf-8');

    // Field names must match db9 CLI exactly
    expect(raw).toContain('token = "tok-123"');
    expect(raw).toContain('is_anonymous = true');
    expect(raw).toContain('anonymous_id = "anon-id"');
    expect(raw).toContain('anonymous_secret = "anon-sec"');
  });

  it('merges with existing file content (read → merge → write)', async () => {
    // Step 1: save token only
    await store.save({ token: 'initial-token' });

    // Step 2: add anonymous fields
    await store.save({
      token: 'initial-token',
      is_anonymous: true,
      anonymous_id: 'id-abc',
      anonymous_secret: 'sec-xyz',
    });

    const after2 = await store.load();
    expect(after2?.token).toBe('initial-token');
    expect(after2?.is_anonymous).toBe(true);
    expect(after2?.anonymous_id).toBe('id-abc');
    expect(after2?.anonymous_secret).toBe('sec-xyz');

    // Step 3: update token only — anon fields must survive
    await store.save({ token: 'new-token' });

    const after3 = await store.load();
    expect(after3?.token).toBe('new-token');
    expect(after3?.is_anonymous).toBe(true);
    expect(after3?.anonymous_id).toBe('id-abc');
    expect(after3?.anonymous_secret).toBe('sec-xyz');
  });

  it('creates parent directory if it does not exist', async () => {
    const nested = path.join(tmpDir, 'nested', 'dir', 'credentials');
    const nestedStore = new FileCredentialStore(nested);
    await nestedStore.save({ token: 'nested-token' });

    const loaded = await nestedStore.load();
    expect(loaded?.token).toBe('nested-token');
  });

  it('clears credentials by removing the file', async () => {
    await store.save({ token: 'doomed' });
    await store.clear();
    expect(await store.load()).toBeNull();

    // The file itself should be gone
    await expect(fs.access(credFile)).rejects.toThrow();
  });

  it('clear on non-existent file does not throw', async () => {
    await expect(store.clear()).resolves.toBeUndefined();
  });

  it('returns null when file exists but has no token field', async () => {
    await fs.writeFile(credFile, 'is_anonymous = true\n');
    expect(await store.load()).toBeNull();
  });

  it('roundtrips boolean false correctly', async () => {
    await store.save({
      token: 'rt-token',
      is_anonymous: false,
      anonymous_id: 'rt-id',
      anonymous_secret: 'rt-secret',
    });

    const loaded = await store.load();
    expect(loaded?.token).toBe('rt-token');
    expect(loaded?.is_anonymous).toBe(false);
    expect(loaded?.anonymous_id).toBe('rt-id');
    expect(loaded?.anonymous_secret).toBe('rt-secret');
  });

  it('preserves extra fields written by other tools', async () => {
    // Simulate another tool writing extra fields
    await fs.writeFile(
      credFile,
      'token = "ext-token"\nextra_field = "keep-me"\n',
    );

    // Save with SDK — should preserve extra_field
    await store.save({ token: 'ext-token', is_anonymous: true });

    const raw = await fs.readFile(credFile, 'utf-8');
    expect(raw).toContain('extra_field = "keep-me"');
    expect(raw).toContain('is_anonymous = true');
  });
});

// ---------------------------------------------------------------------------
// defaultCredentialStore
// ---------------------------------------------------------------------------

describe('defaultCredentialStore', () => {
  it('returns a FileCredentialStore instance', () => {
    const store = defaultCredentialStore();
    expect(store).toBeInstanceOf(FileCredentialStore);
  });
});
