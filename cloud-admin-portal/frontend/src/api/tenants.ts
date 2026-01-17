/**
 * Tenant API hooks
 */

import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query"
import { apiRequest } from "./client"
import type {
  Tenant,
  CreateTenantRequest,
  CreateTenantResponse,
  TenantConnectRequest,
  TenantConnectResponse,
  MessageResponse,
} from "@/types"

export function useTenants() {
  return useQuery({
    queryKey: ["tenants"],
    queryFn: () => apiRequest<Tenant[]>("/tenants"),
  })
}

export function useTenant(name: string) {
  return useQuery({
    queryKey: ["tenants", name],
    queryFn: () => apiRequest<Tenant>(`/tenants/${name}`),
    enabled: !!name,
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
    mutationFn: (name: string) =>
      apiRequest<MessageResponse>(`/tenants/${name}`, { method: "DELETE" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
    },
  })
}

export function useConnectTenant(name: string) {
  return useMutation({
    mutationFn: (data: TenantConnectRequest) =>
      apiRequest<TenantConnectResponse>(`/tenants/${name}/connect`, {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: (data) => {
      // Store session ID for subsequent user management requests
      sessionStorage.setItem("tenant_session", data.session_id)
    },
  })
}
