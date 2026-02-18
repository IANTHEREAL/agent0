import { createHttpClient, type FetchFn, type HttpClient } from './http';
import type { CredentialStore } from './credentials';
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

export interface CustomerClientOptions {
  baseUrl?: string;
  token?: string;
  fetch?: FetchFn;
  credentialStore?: CredentialStore;
}

export function createCustomerClient(options: CustomerClientOptions = {}) {
  const baseUrl =
    options.baseUrl ?? 'https://db9.shared.aws.tidbcloud.com/api';
  let token = options.token;
  let tokenLoaded = !!token;

  // Public HTTP client — no Authorization header
  const publicClient = createHttpClient({
    baseUrl,
    fetch: options.fetch,
  });

  // Lazy-loading authenticated HTTP client
  async function getAuthClient(): Promise<HttpClient> {
    if (!token && !tokenLoaded && options.credentialStore) {
      const creds = await options.credentialStore.load();
      if (creds) token = creds.token;
      tokenLoaded = true;
    }
    if (!token) throw new Error('No authentication token available');
    return createHttpClient({
      baseUrl,
      fetch: options.fetch,
      headers: { Authorization: `Bearer ${token}` },
    });
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
  };
}

export type CustomerClient = ReturnType<typeof createCustomerClient>;
