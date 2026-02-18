import { createHttpClient, type FetchFn } from './http';
import type {
  CreateTenantRequest,
  CreateTenantResponse,
  TenantResponse,
  TenantListResponse,
  TenantUpdateRequest,
  TenantConnectRequest,
  TenantConnectResponse,
  SqlQueryRequest,
  SqlQueryResponse,
  ListTenantsParams,
  MessageResponse,
  HealthResponse,
  BatchCreateRequest,
  BatchCreateResponse,
  BatchDeleteRequest,
  BatchDeleteResponse,
  BatchUpdateRequest,
  BatchUpdateResponse,
  UserResponse,
  UserCreateResponse,
  AdminCreateUserRequest,
  PasswordResetResponse,
  TenantObservabilityResponse,
  AuditLogParams,
  AuditLogResponse,
} from './types';

export interface AdminClientOptions {
  baseUrl?: string;
  apiKey?: string;
  fetch?: FetchFn;
}

export function createAdminClient(options: AdminClientOptions = {}) {
  const baseUrl =
    options.baseUrl ?? 'https://db9.shared.aws.tidbcloud.com/api';
  const headers: Record<string, string> = {};
  if (options.apiKey) {
    headers['X-API-Key'] = options.apiKey;
  }

  const client = createHttpClient({
    baseUrl,
    fetch: options.fetch,
    headers,
  });

  return {
    tenants: {
      list: (params?: ListTenantsParams) => {
        const queryParams: Record<string, string | undefined> = {};
        if (params) {
          if (params.page !== undefined)
            queryParams.page = String(params.page);
          if (params.size !== undefined)
            queryParams.size = String(params.size);
          if (params.state !== undefined) queryParams.state = params.state;
          if (params.q !== undefined) queryParams.q = params.q;
          if (params.cursor !== undefined) queryParams.cursor = params.cursor;
          if (params.tag !== undefined) queryParams.tag = params.tag;
        }
        return client.get<TenantListResponse>('/tenants', queryParams);
      },
      create: (req?: CreateTenantRequest) =>
        client.post<CreateTenantResponse>('/tenants', req),
      get: (tenantId: string) =>
        client.get<TenantResponse>(`/tenants/${tenantId}`),
      update: (tenantId: string, req: TenantUpdateRequest) =>
        client.put<TenantResponse>(`/tenants/${tenantId}`, req),
      delete: (tenantId: string) =>
        client.del<MessageResponse>(`/tenants/${tenantId}`),
      remove: (tenantId: string) =>
        client.post<MessageResponse>(`/tenants/${tenantId}/remove`),

      batchCreate: (req: BatchCreateRequest) =>
        client.post<BatchCreateResponse>('/tenants/batch', req),
      batchDelete: (req: BatchDeleteRequest) =>
        client.post<BatchDeleteResponse>('/tenants/batch/delete', req),
      batchUpdate: (req: BatchUpdateRequest) =>
        client.put<BatchUpdateResponse>('/tenants/batch', req),

      connect: (tenantId: string, req: TenantConnectRequest) =>
        client.post<TenantConnectResponse>(
          `/tenants/${tenantId}/connect`,
          req
        ),

      // query() needs X-Tenant-Session header — create a one-off client
      query: async (
        tenantId: string,
        sessionId: string,
        req: SqlQueryRequest
      ) => {
        const sessionClient = createHttpClient({
          baseUrl,
          fetch: options.fetch,
          headers: {
            ...headers,
            'X-Tenant-Session': sessionId,
          },
        });
        return sessionClient.post<SqlQueryResponse>(
          `/tenants/${tenantId}/query`,
          req
        );
      },
    },

    system: {
      health: () => client.get<HealthResponse>('/health'),
      info: () =>
        client.get<{ name: string; version: string; docs: string }>('/info'),
    },

    users: {
      list: async (tenantId: string, sessionId: string) => {
        const sessionClient = createHttpClient({
          baseUrl,
          fetch: options.fetch,
          headers: {
            ...headers,
            'X-Tenant-Session': sessionId,
          },
        });
        return sessionClient.get<UserResponse[]>(
          `/tenants/${tenantId}/users`
        );
      },

      create: async (
        tenantId: string,
        sessionId: string,
        req: AdminCreateUserRequest
      ) => {
        const sessionClient = createHttpClient({
          baseUrl,
          fetch: options.fetch,
          headers: {
            ...headers,
            'X-Tenant-Session': sessionId,
          },
        });
        return sessionClient.post<UserCreateResponse>(
          `/tenants/${tenantId}/users`,
          req
        );
      },

      delete: async (
        tenantId: string,
        sessionId: string,
        username: string
      ) => {
        const sessionClient = createHttpClient({
          baseUrl,
          fetch: options.fetch,
          headers: {
            ...headers,
            'X-Tenant-Session': sessionId,
          },
        });
        return sessionClient.del<MessageResponse>(
          `/tenants/${tenantId}/users/${username}`
        );
      },

      resetPassword: async (
        tenantId: string,
        sessionId: string,
        username: string
      ) => {
        const sessionClient = createHttpClient({
          baseUrl,
          fetch: options.fetch,
          headers: {
            ...headers,
            'X-Tenant-Session': sessionId,
          },
        });
        return sessionClient.post<PasswordResetResponse>(
          `/tenants/${tenantId}/users/${username}/password`
        );
      },
    },

    observability: {
      get: (tenantId: string) =>
        client.get<TenantObservabilityResponse>(
          `/tenants/${tenantId}/observability`
        ),

      bootstrap: (tenantId: string, req: TenantConnectRequest) =>
        client.post<MessageResponse>(
          `/tenants/${tenantId}/observability/bootstrap`,
          req
        ),
    },

    audit: {
      list: (params?: AuditLogParams) => {
        const queryParams: Record<string, string | undefined> = {};
        if (params) {
          if (params.tenant_id !== undefined)
            queryParams.tenant_id = params.tenant_id;
          if (params.operation_type !== undefined)
            queryParams.operation_type = params.operation_type;
          if (params.resource_type !== undefined)
            queryParams.resource_type = params.resource_type;
          if (params.success !== undefined)
            queryParams.success = String(params.success);
          if (params.limit !== undefined)
            queryParams.limit = String(params.limit);
          if (params.offset !== undefined)
            queryParams.offset = String(params.offset);
        }
        return client.get<AuditLogResponse[]>('/audit-logs', queryParams);
      },
    },
  };
}

export type AdminClient = ReturnType<typeof createAdminClient>;