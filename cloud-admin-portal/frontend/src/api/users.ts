import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query"
import { apiRequest } from "./client"
import type {
  User,
  CreateUserRequest,
  CreateUserResponse,
  PasswordResetResponse,
  MessageResponse,
} from "@/types"

export function useUsers(tenantId: string, enabled: boolean = true) {
  return useQuery({
    queryKey: ["tenants", tenantId, "users"],
    queryFn: () => apiRequest<User[]>(`/tenants/${tenantId}/users`),
    enabled: enabled && !!tenantId,
  })
}

export function useCreateUser(tenantId: string) {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (data: CreateUserRequest) =>
      apiRequest<CreateUserResponse>(`/tenants/${tenantId}/users`, {
        method: "POST",
        body: JSON.stringify(data),
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants", tenantId, "users"] })
    },
  })
}

export function useDeleteUser(tenantId: string) {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: (username: string) =>
      apiRequest<MessageResponse>(`/tenants/${tenantId}/users/${username}`, {
        method: "DELETE",
      }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants", tenantId, "users"] })
    },
  })
}

export function useResetPassword(tenantId: string) {
  return useMutation({
    mutationFn: (username: string) =>
      apiRequest<PasswordResetResponse>(
        `/tenants/${tenantId}/users/${username}/password`,
        { method: "POST" }
      ),
  })
}
