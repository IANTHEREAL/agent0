/**
 * API Client for pg-tikv Admin Portal
 */

const API_BASE = import.meta.env.VITE_API_URL || "/api"

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public details?: Record<string, unknown>
  ) {
    super(message)
    this.name = "ApiError"
  }
}

export async function apiRequest<T>(
  endpoint: string,
  options: RequestInit = {}
): Promise<T> {
  const token = localStorage.getItem("auth_token")
  const tenantSession = sessionStorage.getItem("tenant_session")
  
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...(options.headers as Record<string, string>),
  }
  
  if (token) {
    headers["Authorization"] = `Bearer ${token}`
  }
  if (tenantSession) {
    headers["X-Tenant-Session"] = tenantSession
  }
  
  const response = await fetch(`${API_BASE}${endpoint}`, {
    ...options,
    headers,
  })

  if (!response.ok) {
    const error = await response.json().catch(() => ({ message: "Unknown error" }))
    
    // Handle 401 - redirect to login
    if (response.status === 401 && !endpoint.includes("/auth/")) {
      localStorage.removeItem("auth_token")
      sessionStorage.removeItem("tenant_session")
      window.location.href = "/login"
    }
    
    throw new ApiError(response.status, error.message || error.detail, error.details)
  }

  if (response.status === 204) {
    return undefined as T
  }
  
  return response.json()
}
