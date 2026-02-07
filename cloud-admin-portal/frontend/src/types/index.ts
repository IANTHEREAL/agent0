// API Response Types

export type EndpointType = "primary" | "replica" | "load_balancer"

export interface Endpoint {
  host: string
  port: number
  type: EndpointType
  region?: string | null
  priority: number
  description?: string | null
  enabled: boolean
  connection_string: string
}

export interface Tenant {
  id: string
  state: string
  endpoints: Endpoint[]
  is_deleted?: boolean
  created_at?: string
  created_by?: string | null
  notes?: string | null
  tags?: string[] | null
  updated_at?: string | null
  state_reason?: string | null
}

export interface TenantListResponse {
  items: Tenant[]
  total: number
  page: number
  size: number
}

export interface CreateTenantRequest {
  admin_user?: string
  admin_password?: string
}

export interface CreateTenantResponse {
  id: string
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

export interface SqlQueryRequest {
  sql: string
}

export interface SqlQueryResponse {
  success: boolean
  result?: string | null
  error?: string | null
  rows_affected?: number | null
}

export interface ObservabilitySummary {
  window_seconds: number
  statement_count: number
  txn_commit_count: number
  error_count: number
  qps: number
  tps: number
  latency_avg_ms: number
  latency_p99_ms: number
  active_connections: number
}

export interface QuerySample {
  query: string
  sample_count: number
  error_count: number
  latency_avg_ms: number
  latency_p99_ms: number
  latency_max_ms: number
  last_seen_ms_ago: number
}

export interface TenantObservabilityResponse {
  summary: ObservabilitySummary
  samples: QuerySample[]
}
