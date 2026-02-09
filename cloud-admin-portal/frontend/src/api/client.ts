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
  const tenantMatch = endpoint.match(/^\/tenants\/([^/]+)/)
  const tenantName = tenantMatch ? tenantMatch[1] : null
  const tenantSession = tenantName ? sessionStorage.getItem(`tenant_session:${tenantName}`) : null
  
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...(options.headers as Record<string, string>),
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

    if (response.status === 401 && tenantName) {
      sessionStorage.removeItem(`tenant_session:${tenantName}`)
      window.dispatchEvent(new CustomEvent("session-expired", { detail: { tenantId: tenantName } }))
    }

    throw new ApiError(response.status, error.message || error.detail, error.details)
  }

  if (response.status === 204) {
    return undefined as T
  }
  
  return response.json()
}
