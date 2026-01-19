import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query"
import { ApiError, apiRequest } from "./client"
import type {
  Tenant,
  CreateTenantRequest,
  CreateTenantResponse,
  TenantConnectRequest,
  TenantConnectResponse,
  MessageResponse,
  SqlQueryResponse,
  TenantObservabilityResponse,
} from "@/types"

export function useTenants() {
  return useQuery({
    queryKey: ["tenants"],
    queryFn: () => apiRequest<Tenant[]>("/tenants"),
  })
}

export function useTenant(tenantId: string) {
  return useQuery({
    queryKey: ["tenants", tenantId],
    queryFn: () => apiRequest<Tenant>(`/tenants/${tenantId}`),
    enabled: !!tenantId,
  })
}

export function useCreateTenant() {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (data: CreateTenantRequest) =>
      apiRequest<CreateTenantResponse>("/tenants", {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
    },
  })
}

export function useDeleteTenant() {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (tenantId: string) =>
      apiRequest<MessageResponse>(`/tenants/${tenantId}`, { method: "DELETE" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
    },
  })
}

export function useRemoveTenant() {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (tenantId: string) =>
      apiRequest<MessageResponse>(`/tenants/${tenantId}/remove`, { method: "POST" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
    },
  })
}

export function useUpdateTenant(tenantId: string) {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (data: { notes?: string | null; tags?: string[] | null }) =>
      apiRequest<Tenant>(`/tenants/${tenantId}`, {
        method: "PUT",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
      queryClient.invalidateQueries({ queryKey: ["tenants", tenantId] })
    },
  })
}

export function useConnectTenant(tenantId: string) {
  return useMutation({
    mutationFn: (data: TenantConnectRequest) =>
      apiRequest<TenantConnectResponse>(`/tenants/${tenantId}/connect`, {
        method: "POST",
        body: JSON.stringify(data),
      }),
  })
}

export function useExecuteQuery(tenantId: string) {
  return useMutation({
    mutationFn: (sql: string) =>
      apiRequest<SqlQueryResponse>(`/tenants/${tenantId}/query`, {
        method: "POST",
        body: JSON.stringify({ sql }),
      }),
  })
}

export function useTenantObservability(tenantId: string) {
  return useQuery({
    queryKey: ["tenants", tenantId, "observability"],
    queryFn: () =>
      apiRequest<TenantObservabilityResponse>(`/tenants/${tenantId}/observability`),
    enabled: !!tenantId,
    retry: (failureCount, error) => {
      if (error instanceof ApiError && error.status === 409) {
        return false
      }
      return failureCount < 2
    },
    refetchInterval: (query) => {
      const error = query.state.error
      if (error instanceof ApiError && error.status === 409) {
        return false
      }
      return 5000
    },
  })
}

export function bootstrapTenantObservabilityUser(
  tenantId: string,
  adminUser: string,
  adminPassword: string
) {
  return apiRequest<MessageResponse>(`/tenants/${tenantId}/observability/bootstrap`, {
    method: "POST",
    body: JSON.stringify({
      admin_user: adminUser,
      admin_password: adminPassword,
    }),
  })
}
