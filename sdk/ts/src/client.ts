import { createHttpClient, type FetchFn, type HttpClient, type BodyInit } from './http';
import {
  defaultCredentialStore,
  type CredentialStore,
} from './credentials';
import { Db9Error } from './errors';
import type { Fs9FileEntry, Fs9ListOptions } from './fs-types';
import type {
  RegisterRequest,
  CustomerResponse,
  LoginRequest,
  LoginResponse,
  AnonymousRegisterResponse,
  AnonymousRefreshRequest,
  AnonymousRefreshResponse,
  AnonymousSecretResponse,
  ClaimRequest,
  ClaimResponse,
  TokenResponse,
  MessageResponse,
  CreateDatabaseRequest,
  DatabaseResponse,
  CustomerPasswordResetResponse,
  TenantObservabilityResponse,
  SqlResult,
  SqlErrorDetail,
  SchemaResponse,
  DumpRequest,
  DumpResponse,
  MigrationApplyRequest,
  MigrationApplyResponse,
  MigrationMetadata,
  BranchRequest,
  UserResponse,
  CreateUserRequest,
  CreateTokenRequest,
  CreateTokenResponse,
  Fs9EventEntry,
  Fs9EventOptions,
} from './types';

export interface Db9ClientOptions {
  baseUrl?: string;
  token?: string;
  fetch?: FetchFn;
  credentialStore?: CredentialStore;
  timeout?: number;
  maxRetries?: number;
  retryDelay?: number;
}

export function createDb9Client(options: Db9ClientOptions = {}) {
  const baseUrl =
    options.baseUrl ?? 'https://db9.shared.aws.tidbcloud.com/api';
  let token = options.token;
  let tokenLoaded = !!token;
  const store = options.credentialStore ?? defaultCredentialStore();
  const fetchFn = options.fetch ?? globalThis.fetch;

  // Public HTTP client — no Authorization header
  const publicClient = createHttpClient({
    baseUrl,
    fetch: options.fetch,
    timeout: options.timeout,
    maxRetries: options.maxRetries,
    retryDelay: options.retryDelay,
  });

  // Lazy-loading authenticated HTTP client
  async function getAuthClient(): Promise<HttpClient> {
    if (!token && !tokenLoaded) {
      const creds = await store.load();
      if (creds?.token) token = creds.token;
      tokenLoaded = true;
    }
    if (!token) {
      const reg = await publicClient.post<AnonymousRegisterResponse>(
        '/customer/anonymous-register'
      );
      token = reg.token;
      await store.save({
        token: reg.token,
        is_anonymous: reg.is_anonymous,
        anonymous_id: reg.anonymous_id,
        anonymous_secret: reg.anonymous_secret,
      });
    }
    return createHttpClient({
      baseUrl,
      fetch: options.fetch,
      headers: { Authorization: `Bearer ${token}` },
      timeout: options.timeout,
      maxRetries: options.maxRetries,
      retryDelay: options.retryDelay,
    });
  }

  // ── Token auto-refresh on 401 ─────────────────────────────────
  let refreshPromise: Promise<void> | null = null;

  async function refreshAnonymousToken(): Promise<void> {
    const creds = await store.load();
    if (!creds?.anonymous_id || !creds?.anonymous_secret) {
      throw new Error('Not an anonymous session');
    }
    const resp = await publicClient.post<AnonymousRefreshResponse>(
      '/customer/anonymous-refresh',
      {
        anonymous_id: creds.anonymous_id,
        anonymous_secret: creds.anonymous_secret,
      }
    );
    token = resp.token;
    await store.save({ ...creds, token: resp.token });
  }

  async function withAuthRetry<T>(
    operation: (client: HttpClient) => Promise<T>
  ): Promise<T> {
    const client = await getAuthClient();
    try {
      return await operation(client);
    } catch (err) {
      if (!(err instanceof Db9Error) || err.statusCode !== 401) {
        throw err;
      }
      // 401 — try anonymous refresh
      try {
        if (!refreshPromise) {
          refreshPromise = refreshAnonymousToken();
        }
        await refreshPromise;
      } catch {
        throw err; // Refresh failed — throw original 401
      } finally {
        refreshPromise = null;
      }
      // Retry with new token
      const newClient = await getAuthClient();
      return operation(newClient);
    }
  }

  // ── fs9 helpers ──────────────────────────────────────────────
  function deriveFs9Url(dbId: string): string {
    const origin = baseUrl.replace(/\/api\/?$/, '');
    return `${origin}/fs9/${dbId}`;
  }

  // ── FS-specific auth retry (shares refreshPromise singleton) ──
  function getFsClient(dbId: string): HttpClient {
    const fs9Base = deriveFs9Url(dbId) + '/api/v1';
    return createHttpClient({
      baseUrl: fs9Base,
      fetch: options.fetch,
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      timeout: options.timeout,
      maxRetries: options.maxRetries,
      retryDelay: options.retryDelay,
    });
  }

  async function withFsAuthRetry<T>(
    dbId: string,
    operation: (client: HttpClient) => Promise<T>
  ): Promise<T> {
    // Ensure token is loaded first
    if (!token && !tokenLoaded) {
      await getAuthClient(); // triggers lazy auth
    }
    const client = getFsClient(dbId);
    try {
      return await operation(client);
    } catch (err) {
      if (!(err instanceof Db9Error) || err.statusCode !== 401) {
        throw err;
      }
      try {
        if (!refreshPromise) {
          refreshPromise = refreshAnonymousToken();
        }
        await refreshPromise;
      } catch {
        throw err;
      } finally {
        refreshPromise = null;
      }
      const newClient = getFsClient(dbId);
      return operation(newClient);
    }
  }

  // ── SQL Error Parsing ────────────────────────────────────────
  function parseSqlError(raw: string): SqlErrorDetail {
    // Strategy 1: Try JSON.parse
    try {
      const parsed = JSON.parse(raw);
      if (typeof parsed === 'object' && parsed !== null && typeof parsed.message === 'string') {
        return parsed as SqlErrorDetail;
      }
    } catch {
      // not JSON, continue
    }

    // Strategy 2: Regex for PostgreSQL-style errors
    const pgMatch = raw.match(/^(?:ERROR:\s*)?(.+?)(?:\s+DETAIL:\s+(.+?))?(?:\s+HINT:\s+(.+?))?(?:\s+\(SQLSTATE\s+(\w+)\))?$/s);
    if (pgMatch && pgMatch[1]) {
      const result: SqlErrorDetail = { message: pgMatch[1].trim() };
      if (pgMatch[2]) result.detail = pgMatch[2].trim();
      if (pgMatch[3]) result.hint = pgMatch[3].trim();
      if (pgMatch[4]) result.code = pgMatch[4];
      return result;
    }

    // Strategy 3: Fallback
    return { message: raw };
  }

  async function fsStat(dbId: string, path: string): Promise<Fs9FileEntry> {
    return withFsAuthRetry(dbId, (client) =>
      client.get<Fs9FileEntry>('/stat', { path })
    );
  }

  // ── Anonymous Secret Helper ──────────────────────────────────
  async function fetchAnonymousSecret(): Promise<AnonymousSecretResponse> {
    return withAuthRetry((client) =>
      client.post<AnonymousSecretResponse>('/customer/anonymous-secret', {})
    );
  }

  return {
    auth: {
      // Public endpoints (no token required)
      register: (req: RegisterRequest) =>
        publicClient.post<CustomerResponse>('/customer/register', req),

      login: (req: LoginRequest) =>
        publicClient.post<LoginResponse>('/customer/login', req),

      anonymousRegister: () =>
        publicClient.post<AnonymousRegisterResponse>(
          '/customer/anonymous-register'
        ),

      anonymousRefresh: (req: AnonymousRefreshRequest) =>
        publicClient.post<AnonymousRefreshResponse>(
          '/customer/anonymous-refresh',
          req
        ),

      // Authenticated endpoints
      me: async () =>
        withAuthRetry((client) =>
          client.get<CustomerResponse>('/customer/me')
        ),

      getAnonymousSecret: (): Promise<AnonymousSecretResponse> => {
        return fetchAnonymousSecret();
      },

      claim: async (req: ClaimRequest) =>
        withAuthRetry((client) =>
          client.post<ClaimResponse>('/customer/claim', req)
        ),

      ensureAnonymousSecret: async () => {
        const creds = await store.load();
        if (!creds?.anonymous_id || creds.anonymous_secret) {
          return; // No anonymous_id, or secret already exists
        }
        // Anonymous session without a secret — fetch one
        const resp = await fetchAnonymousSecret();
        await store.save({
          ...creds,
          anonymous_secret: resp.anonymous_secret,
        });
      },
    },

    tokens: {
      list: async () =>
        withAuthRetry((client) =>
          client.get<TokenResponse[]>('/customer/tokens')
        ),

      revoke: async (tokenId: string) =>
        withAuthRetry((client) =>
          client.del<MessageResponse>(`/customer/tokens/${tokenId}`)
        ),

      create: async (req: CreateTokenRequest) =>
        withAuthRetry((client) =>
          client.post<CreateTokenResponse>('/customer/tokens', req)
        ),
    },

    databases: {
      // ── CRUD ──────────────────────────────────────────────────
      create: async (req: CreateDatabaseRequest) =>
        withAuthRetry((client) =>
          client.post<DatabaseResponse>('/customer/databases', req)
        ),

      list: async () =>
        withAuthRetry((client) =>
          client.get<DatabaseResponse[]>('/customer/databases')
        ),

      get: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.get<DatabaseResponse>(
            `/customer/databases/${databaseId}`
          )
        ),

      delete: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.del<MessageResponse>(
            `/customer/databases/${databaseId}`
          )
        ),

      resetPassword: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.post<CustomerPasswordResetResponse>(
            `/customer/databases/${databaseId}/reset-password`
          )
        ),

      observability: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.get<TenantObservabilityResponse>(
            `/customer/databases/${databaseId}/observability`
          )
        ),

      // ── SQL Execution ─────────────────────────────────────────
      sql: async (databaseId: string, query: string) => {
        const result = await withAuthRetry((client) =>
          client.post<SqlResult>(
            `/customer/databases/${databaseId}/sql`,
            { query }
          )
        );
        if (result.error && typeof result.error === 'string') {
          result.error = parseSqlError(result.error);
        }
        return result;
      },

      sqlFile: async (databaseId: string, fileContent: string) => {
        const result = await withAuthRetry((client) =>
          client.post<SqlResult>(
            `/customer/databases/${databaseId}/sql`,
            { file_content: fileContent }
          )
        );
        if (result.error && typeof result.error === 'string') {
          result.error = parseSqlError(result.error);
        }
        return result;
      },

      // ── Schema & Dump ─────────────────────────────────────────
      schema: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.get<SchemaResponse>(
            `/customer/databases/${databaseId}/schema`
          )
        ),

      dump: async (databaseId: string, req?: DumpRequest) =>
        withAuthRetry((client) =>
          client.post<DumpResponse>(
            `/customer/databases/${databaseId}/dump`,
            req
          )
        ),

      // ── Migrations ────────────────────────────────────────────
      applyMigration: async (
        databaseId: string,
        req: MigrationApplyRequest
      ) =>
        withAuthRetry((client) =>
          client.post<MigrationApplyResponse>(
            `/customer/databases/${databaseId}/migrations`,
            req
          )
        ),

      listMigrations: async (databaseId: string) =>
        withAuthRetry((client) =>
          client.get<MigrationMetadata[]>(
            `/customer/databases/${databaseId}/migrations`
          )
        ),

      // ── Branching ─────────────────────────────────────────────
      branch: async (databaseId: string, req: BranchRequest) =>
        withAuthRetry((client) =>
          client.post<DatabaseResponse>(
            `/customer/databases/${databaseId}/branch`,
            req
          )
        ),

      // ── User Management ───────────────────────────────────────
      users: {
        list: async (databaseId: string) =>
          withAuthRetry((client) =>
            client.get<UserResponse[]>(
              `/customer/databases/${databaseId}/users`
            )
          ),

        create: async (databaseId: string, req: CreateUserRequest) =>
          withAuthRetry((client) =>
            client.post<MessageResponse>(
              `/customer/databases/${databaseId}/users`,
              req
            )
          ),

        delete: async (databaseId: string, username: string) =>
          withAuthRetry((client) =>
            client.del<MessageResponse>(
              `/customer/databases/${databaseId}/users/${username}`
            )
          ),
      },
    },

    fs: {
      list: async (
        dbId: string,
        path: string,
        options?: Fs9ListOptions
      ): Promise<Fs9FileEntry[]> => {
        const params: Record<string, string | undefined> = { path };
        if (options?.recursive) params.recursive = 'true';
        return withFsAuthRetry(dbId, (client) =>
          client.get<Fs9FileEntry[]>('/readdir', params)
        );
      },

      read: async (dbId: string, path: string): Promise<string> => {
        return withFsAuthRetry(dbId, async (client) => {
          const resp = await client.getRaw('/download', { path });
          return resp.text();
        });
      },

      readBinary: async (dbId: string, path: string): Promise<ArrayBuffer> => {
        return withFsAuthRetry(dbId, async (client) => {
          const resp = await client.getRaw('/download', { path });
          return resp.arrayBuffer();
        });
      },

      write: async (
        dbId: string,
        path: string,
        content: string | ArrayBuffer | Uint8Array | Blob
      ): Promise<void> => {
        const contentType = typeof content === 'string' ? 'text/plain' : 'application/octet-stream';
        await withFsAuthRetry(dbId, (client) =>
          client.putRaw(`/upload?${new URLSearchParams({ path })}`, content as BodyInit, { 'Content-Type': contentType })
        );
      },

      stat: (dbId: string, path: string): Promise<Fs9FileEntry> => {
        return fsStat(dbId, path);
      },

      exists: async (dbId: string, path: string): Promise<boolean> => {
        try {
          await fsStat(dbId, path);
          return true;
        } catch (err) {
          if (err instanceof Db9Error && err.statusCode === 404) {
            return false;
          }
          throw err;
        }
      },

      mkdir: async (dbId: string, path: string): Promise<void> => {
        await withFsAuthRetry(dbId, (client) =>
          client.postRaw(`/mkdir?${new URLSearchParams({ path, recursive: 'true' })}`)
        );
      },

      remove: async (dbId: string, path: string): Promise<void> => {
        await withFsAuthRetry(dbId, (client) =>
          client.delRaw('/remove', { path })
        );
      },

      events: async (
        dbId: string,
        options?: Fs9EventOptions
      ): Promise<Fs9EventEntry[]> => {
        const params: Record<string, string | undefined> = {};
        if (options?.limit !== undefined) params.limit = String(options.limit);
        if (options?.offset !== undefined) params.offset = String(options.offset);
        if (options?.path) params.path = options.path;
        if (options?.type) params.type = options.type;
        return withFsAuthRetry(dbId, (client) =>
          client.get<Fs9EventEntry[]>('/events', params)
        );
      },
    },
  };
}

export type Db9Client = ReturnType<typeof createDb9Client>;
