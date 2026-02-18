import { parse as parseToml, stringify as stringifyToml } from '@iarna/toml';

// ---------------------------------------------------------------------------
// Interfaces
// ---------------------------------------------------------------------------

/** Credential fields stored in `~/.db9/credentials` (TOML). */
export interface Credentials {
  token: string;
  is_anonymous?: boolean;
  anonymous_id?: string;
  anonymous_secret?: string;
}

/** Async credential persistence abstraction. */
export interface CredentialStore {
  load(): Promise<Credentials | null>;
  save(credentials: Credentials): Promise<void>;
  clear(): Promise<void>;
}

// ---------------------------------------------------------------------------
// FileCredentialStore — TOML file at ~/.db9/credentials (matches db9 CLI)
// ---------------------------------------------------------------------------

export class FileCredentialStore implements CredentialStore {
  private readonly customPath: string | undefined;

  /**
   * @param path — Override the credential file location.
   *               Defaults to `~/.db9/credentials` (resolved lazily).
   */
  constructor(path?: string) {
    this.customPath = path;
  }

  /** Resolve the credential file path (lazy to avoid top-level `os` import). */
  private async resolvePath(): Promise<string> {
    if (this.customPath) return this.customPath;
    const os = await import('node:os');
    const nodePath = await import('node:path');
    return nodePath.join(os.homedir(), '.db9', 'credentials');
  }

  async load(): Promise<Credentials | null> {
    const fs = await import('node:fs/promises');
    const filePath = await this.resolvePath();

    let content: string;
    try {
      content = await fs.readFile(filePath, 'utf-8');
    } catch (err: unknown) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return null;
      throw err;
    }

    const parsed = parseToml(content);
    const token = parsed['token'];
    if (typeof token !== 'string') return null;

    const creds: Credentials = { token };
    if (typeof parsed['is_anonymous'] === 'boolean') {
      creds.is_anonymous = parsed['is_anonymous'];
    }
    if (typeof parsed['anonymous_id'] === 'string') {
      creds.anonymous_id = parsed['anonymous_id'];
    }
    if (typeof parsed['anonymous_secret'] === 'string') {
      creds.anonymous_secret = parsed['anonymous_secret'];
    }

    return creds;
  }

  async save(credentials: Credentials): Promise<void> {
    const fs = await import('node:fs/promises');
    const nodePath = await import('node:path');
    const filePath = await this.resolvePath();
    const dir = nodePath.dirname(filePath);

    // Ensure directory exists with 0o700 (matches CLI: ensure_config_dir)
    await fs.mkdir(dir, { recursive: true, mode: 0o700 });

    // Read → merge → write  (preserves unknown fields as scalars)
    const data: Record<string, string | boolean | number> = {};
    try {
      const raw = await fs.readFile(filePath, 'utf-8');
      const parsed = parseToml(raw);
      for (const [k, v] of Object.entries(parsed)) {
        if (typeof v === 'string' || typeof v === 'boolean' || typeof v === 'number') {
          data[k] = v;
        }
      }
    } catch (err: unknown) {
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    }

    // Merge: always update token, only override optional fields when provided
    data['token'] = credentials.token;
    if (credentials.is_anonymous !== undefined) {
      data['is_anonymous'] = credentials.is_anonymous;
    }
    if (credentials.anonymous_id !== undefined) {
      data['anonymous_id'] = credentials.anonymous_id;
    }
    if (credentials.anonymous_secret !== undefined) {
      data['anonymous_secret'] = credentials.anonymous_secret;
    }

    // Serialize and write with 0o600 permissions (matches CLI: save_token)
    const toml = stringifyToml(
      data as Parameters<typeof stringifyToml>[0],
    );
    await fs.writeFile(filePath, toml, { mode: 0o600 });
  }

  async clear(): Promise<void> {
    const fs = await import('node:fs/promises');
    const filePath = await this.resolvePath();

    try {
      await fs.unlink(filePath);
    } catch (err: unknown) {
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    }
  }
}

// ---------------------------------------------------------------------------
// MemoryCredentialStore — in-memory, no persistence
// ---------------------------------------------------------------------------

export class MemoryCredentialStore implements CredentialStore {
  private credentials: Credentials | null = null;

  async load(): Promise<Credentials | null> {
    return this.credentials ? { ...this.credentials } : null;
  }

  async save(credentials: Credentials): Promise<void> {
    this.credentials = {
      token: credentials.token,
      is_anonymous: credentials.is_anonymous ?? this.credentials?.is_anonymous,
      anonymous_id: credentials.anonymous_id ?? this.credentials?.anonymous_id,
      anonymous_secret: credentials.anonymous_secret ?? this.credentials?.anonymous_secret,
    };
  }

  async clear(): Promise<void> {
    this.credentials = null;
  }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/** Returns a FileCredentialStore with the default path (`~/.db9/credentials`). */
export function defaultCredentialStore(): CredentialStore {
  return new FileCredentialStore();
}
