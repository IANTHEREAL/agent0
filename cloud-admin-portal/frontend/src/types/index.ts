// API Response Types

export interface Tenant {
  name: string
  state: string
  host?: string
  port?: number
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

export interface CreateUserResponse {
  username: string
  password: string
  connection: string
}

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
