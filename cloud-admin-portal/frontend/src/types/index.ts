// API Response Types

export interface Tenant {
  name: string
  state: string
  host?: string
  port?: number
  is_deleted?: boolean
  created_at?: string
  created_by?: string | null
  notes?: string | null
  tags?: string[] | null
  updated_at?: string | null
}

export interface CreateTenantRequest {
  name: string
  admin_user?: string
  admin_password?: string
}

export interface CreateTenantResponse {
  name: string
  admin_user: string
  admin_password: string
  connection_string: string
  created_at: string
}

export interface TenantConnectRequest {
  admin_user: string
  admin_password: string
}

export interface TenantConnectResponse {
  session_id: string
  expires_at: string
}

export interface User {
  name: string
  is_superuser: boolean
  can_login: boolean
  can_create_db: boolean
  can_create_role: boolean
}

export interface CreateUserRequest {
  username: string
  password?: string
  superuser?: boolean
}

export interface UserCreateResponse {
  username: string
  password: string
  connection: string
}

// Alias for backwards compatibility
export type CreateUserResponse = UserCreateResponse

// Alias for TenantCreateResponse (already defined above)
export type TenantCreateResponse = CreateTenantResponse

export interface PasswordResetResponse {
  username: string
  password: string
}

export interface HealthResponse {
  status: string
  pd_healthy: boolean
}

export interface MessageResponse {
  message: string
}

export interface ApiError {
  error: string
  message: string
  details?: Record<string, unknown>
}
