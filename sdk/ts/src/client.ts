import { createHttpClient, type FetchFn, type HttpClient } from './http';
import {
  defaultCredentialStore,
  type CredentialStore,
} from './credentials';
import { Db9Error } from './errors';
import type { Fs9FileInfo, Fs9StatResponse, Fs9ListOptions } from './fs-types';
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
  SchemaResponse,
  DumpRequest,
  DumpResponse,
  MigrationApplyRequest,
  MigrationApplyResponse,
  MigrationMetadata,
  BranchRequest,
  UserResponse,
  CreateUserRequest,
} from './types';

export interface Db9ClientOptions {
  baseUrl?: string;
  token?: string;
  fetch?: FetchFn;
  credentialStore?: CredentialStore;
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
    });
  }

  // ── fs9 helpers ──────────────────────────────────────────────
  function deriveFs9Url(dbId: string): string {
    const origin = baseUrl.replace(/\/api\/?$/, '');
    return `${origin}/fs9/${dbId}`;
  }

  async function fsRequest(
    method: string,
    dbId: string,
    fsPath: string,
    body?: string
  ): Promise<Response> {
    // Ensure token is loaded (lazy auth pattern)
    if (!token && !tokenLoaded) {
      await getAuthClient();
    }

    const fs9Url = deriveFs9Url(dbId);
    const url = `${fs9Url}/api/v1${fsPath}`;

    const headers: Record<string, string> = {};
    if (token) {
      headers['Authorization'] = `Bearer ${token}`;
    }
    if (body !== undefined) {
      headers['Content-Type'] = 'text/plain';
    }

    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      init.body = body;
    }

    const response = await fetchFn(url, init);
    if (!response.ok) {
      throw await Db9Error.fromResponse(response);
    }
    return response;
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
      me: async () => {
        const client = await getAuthClient();
        return client.get<CustomerResponse>('/customer/me');
      },

      getAnonymousSecret: async () => {
        const client = await getAuthClient();
        return client.get<AnonymousSecretResponse>(
          '/customer/anonymous-secret'
        );
      },

      claim: async (req: ClaimRequest) => {
        const client = await getAuthClient();
        return client.post<ClaimResponse>('/customer/claim', req);
      },
    },

    tokens: {
      list: async () => {
        const client = await getAuthClient();
        return client.get<TokenResponse[]>('/customer/tokens');
      },

      revoke: async (tokenId: string) => {
        const client = await getAuthClient();
        return client.del<MessageResponse>(`/customer/tokens/${tokenId}`);
      },
    },

    databases: {
      // ── CRUD ──────────────────────────────────────────────────
      create: async (req: CreateDatabaseRequest) => {
        const client = await getAuthClient();
        return client.post<DatabaseResponse>('/customer/databases', req);
      },

      list: async () => {
        const client = await getAuthClient();
        return client.get<DatabaseResponse[]>('/customer/databases');
      },

      get: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.get<DatabaseResponse>(
          `/customer/databases/${databaseId}`
        );
      },

      delete: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.del<MessageResponse>(
          `/customer/databases/${databaseId}`
        );
      },

      resetPassword: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.post<CustomerPasswordResetResponse>(
          `/customer/databases/${databaseId}/reset-password`
        );
      },

      observability: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.get<TenantObservabilityResponse>(
          `/customer/databases/${databaseId}/observability`
        );
      },

      // ── SQL Execution ─────────────────────────────────────────
      sql: async (databaseId: string, query: string) => {
        const client = await getAuthClient();
        return client.post<SqlResult>(
          `/customer/databases/${databaseId}/sql`,
          { query }
        );
      },

      sqlFile: async (databaseId: string, fileContent: string) => {
        const client = await getAuthClient();
        return client.post<SqlResult>(
          `/customer/databases/${databaseId}/sql`,
          { file_content: fileContent }
        );
      },

      // ── Schema & Dump ─────────────────────────────────────────
      schema: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.get<SchemaResponse>(
          `/customer/databases/${databaseId}/schema`
        );
      },

      dump: async (databaseId: string, req?: DumpRequest) => {
        const client = await getAuthClient();
        return client.post<DumpResponse>(
          `/customer/databases/${databaseId}/dump`,
          req
        );
      },

      // ── Migrations ────────────────────────────────────────────
      applyMigration: async (
        databaseId: string,
        req: MigrationApplyRequest
      ) => {
        const client = await getAuthClient();
        return client.post<MigrationApplyResponse>(
          `/customer/databases/${databaseId}/migrations`,
          req
        );
      },

      listMigrations: async (databaseId: string) => {
        const client = await getAuthClient();
        return client.get<MigrationMetadata[]>(
          `/customer/databases/${databaseId}/migrations`
        );
      },

      // ── Branching ─────────────────────────────────────────────
      branch: async (databaseId: string, req: BranchRequest) => {
        const client = await getAuthClient();
        return client.post<DatabaseResponse>(
          `/customer/databases/${databaseId}/branch`,
          req
        );
      },

      // ── User Management ───────────────────────────────────────
      users: {
        list: async (databaseId: string) => {
          const client = await getAuthClient();
          return client.get<UserResponse[]>(
            `/customer/databases/${databaseId}/users`
          );
        },

        create: async (databaseId: string, req: CreateUserRequest) => {
          const client = await getAuthClient();
          return client.post<MessageResponse>(
            `/customer/databases/${databaseId}/users`,
            req
          );
        },

        delete: async (databaseId: string, username: string) => {
          const client = await getAuthClient();
          return client.del<MessageResponse>(
            `/customer/databases/${databaseId}/users/${username}`
          );
        },
      },
    },

    fs: {
      list: async (
        dbId: string,
        path: string,
        options?: Fs9ListOptions
      ): Promise<Fs9FileInfo[]> => {
        const params = new URLSearchParams({ path });
        if (options?.recursive) params.set('recursive', 'true');
        const response = await fsRequest(
          'GET',
          dbId,
          `/readdir?${params.toString()}`
        );
        return response.json() as Promise<Fs9FileInfo[]>;
      },

      read: async (dbId: string, path: string): Promise<string> => {
        const params = new URLSearchParams({ path });
        const response = await fsRequest(
          'GET',
          dbId,
          `/download?${params.toString()}`
        );
        return response.text();
      },

      write: async (
        dbId: string,
        path: string,
        content: string
      ): Promise<void> => {
        const params = new URLSearchParams({ path });
        await fsRequest('PUT', dbId, `/upload?${params.toString()}`, content);
      },

      stat: async (dbId: string, path: string): Promise<Fs9StatResponse> => {
        const params = new URLSearchParams({ path });
        const response = await fsRequest(
          'GET',
          dbId,
          `/stat?${params.toString()}`
        );
        return response.json() as Promise<Fs9StatResponse>;
      },
    },
  };
}

export type Db9Client = ReturnType<typeof createDb9Client>;
