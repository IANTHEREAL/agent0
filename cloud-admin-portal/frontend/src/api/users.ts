/**
 * User management API hooks
 */

import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query"
import { apiRequest } from "./client"
import type {
  User,
  CreateUserRequest,
  CreateUserResponse,
  PasswordResetResponse,
  MessageResponse,
} from "@/types"

export function useUsers(tenantName: string, enabled: boolean = true) {
  return useQuery({
    queryKey: ["tenants", tenantName, "users"],
    queryFn: () => apiRequest<User[]>(`/tenants/${tenantName}/users`),
    enabled: enabled && !!tenantName,
  })
}

export function useCreateUser(tenantName: string) {
  const queryClient = useQueryClient()
  
  return useMutation({
    mutationFn: (data: CreateUserRequest) =>
      apiRequest<CreateUserResponse>(`/tenants/${tenantName}/users`, {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants", tenantName, "users"] })
    },
  })
}

export function useDeleteUser(tenantName: string) {
  const queryClient = useQueryClient()
  
  return useMutation({
    mutationFn: (username: string) =>
      apiRequest<MessageResponse>(`/tenants/${tenantName}/users/${username}`, {
        method: "DELETE",
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants", tenantName, "users"] })
    },
  })
}

export function useResetPassword(tenantName: string) {
  return useMutation({
    mutationFn: (username: string) =>
      apiRequest<PasswordResetResponse>(
        `/tenants/${tenantName}/users/${username}/password`,
        { method: "POST" }
      ),
  })
}
