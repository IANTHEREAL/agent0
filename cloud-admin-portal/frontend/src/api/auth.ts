/**
 * Authentication API hooks
 */

import { useMutation } from "@tanstack/react-query"
import { apiRequest } from "./client"
import type { LoginResponse } from "@/types"

interface LoginRequest {
  password: string
}

export function useLogin() {
  return useMutation({
    mutationFn: (data: LoginRequest) =>
      apiRequest<LoginResponse>("/auth/login", {
        method: "POST",
        body: JSON.stringify(data),
      }),
  })
}
