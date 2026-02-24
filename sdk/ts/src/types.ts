// ── Common types ───────────────────────────────────────────────

export interface Endpoint {
  host: string;
  port: number;
  type: string;
  region?: string;
  priority: number;
  description?: string;
  enabled: boolean;
}

export interface MessageResponse {
  message: string;
}

export interface HealthResponse {
  status: string;
  pd_healthy: boolean;
}

export interface ColumnInfo {
  name: string;
  type: string;
}

export interface SqlErrorDetail {
  message: string;
  code?: string;
  position?: number;
  hint?: string;
  detail?: string;
}

export interface SqlResult {
  columns: ColumnInfo[];
  rows: unknown[][];
  row_count: number;
  command: string;
  error?: string | SqlErrorDetail;
}

// ── Admin request types ────────────────────────────────────────

export interface CreateTenantRequest {
  admin_user?: string;
  admin_password?: string;
}

export interface TenantConnectRequest {
  admin_user: string;
  admin_password: string;
}

export interface TenantUpdateRequest {
  notes?: string;
  tags?: string[];
}

export interface AdminCreateUserRequest {
  username: string;
  password?: string;
  superuser?: boolean;
}

export interface SqlQueryRequest {
  sql: string;
}

export interface ListTenantsParams {
  page?: number;
  size?: number;
  state?: string;
  q?: string;
  cursor?: string;
  tag?: string;
}

export interface BatchCreateRequest {
  count: number;
  admin_user?: string;
  admin_password?: string;
}

export interface BatchDeleteRequest {
  ids: string[];
}

export interface BatchUpdateRequest {
  ids: string[];
  notes?: string;
  tags?: string[];
}

export interface AuditLogParams {
  tenant_id?: string;
  operation_type?: string;
  resource_type?: string;
  success?: boolean;
  limit?: number;
  offset?: number;
}

// ── Admin response types ───────────────────────────────────────

export interface TenantResponse {
  id: string;
  state: string;
  created_at?: string;
  created_by?: string;
  notes?: string;
  tags?: string[];
  updated_at?: string;
  state_reason?: string;
  endpoints?: Endpoint[];
}

export interface TenantListResponse {
  items: TenantResponse[];
  total: number;
  page: number;
  size: number;
  next_cursor?: string;
}

export interface CreateTenantResponse {
  id: string;
  admin_user: string;
  admin_password: string;
  connection_string: string;
  created_at: string;
}

export interface TenantConnectResponse {
  session_id: string;
  expires_at: string;
}

export interface BatchCreateResponse {
  created: CreateTenantResponse[];
  failed: BatchItemError[];
  total_requested: number;
  total_created: number;
}

export interface BatchDeleteResponse {
  deleted: string[];
  failed: BatchItemError[];
}

export interface BatchUpdateResponse {
  updated: string[];
  failed: BatchItemError[];
}

export interface BatchItemError {
  id: string;
  error: string;
}

export interface UserResponse {
  name: string;
  is_superuser: boolean;
  can_login: boolean;
  can_create_db: boolean;
  can_create_role: boolean;
}

export interface UserCreateResponse {
  username: string;
  password: string;
  connection: string;
}

export interface PasswordResetResponse {
  username: string;
  password: string;
}

export interface SqlQueryResponse {
  success: boolean;
  result?: string;
  error?: string;
}

export interface AuditLogResponse {
  id: string;
  timestamp: string;
  operation_type: string;
  resource_type: string;
  resource_name: string;
  tenant_id?: string;
  operator?: string;
  success: boolean;
  error_message?: string;
  extra_metadata?: unknown;
}

export interface ObservabilitySummary {
  window_seconds: number;
  statement_count: number;
  txn_commit_count: number;
  error_count: number;
  qps: number;
  tps: number;
  latency_avg_ms: number;
  latency_p99_ms: number;
  active_connections: number;
}

export interface QuerySample {
  query: string;
  sample_count: number;
  error_count: number;
  latency_avg_ms: number;
  latency_p99_ms: number;
  latency_max_ms: number;
  last_seen_ms_ago: number;
}

export interface TenantObservabilityResponse {
  summary: ObservabilitySummary;
  samples: QuerySample[];
}

// ── Customer request types ─────────────────────────────────────

export interface RegisterRequest {
  email: string;
  password: string;
}

export interface LoginRequest {
  email: string;
  password: string;
}

export interface CreateDatabaseRequest {
  name: string;
  region?: string;
  admin_password?: string;
}

export interface SqlExecuteRequest {
  query?: string;
  file_content?: string;
}

export interface DumpRequest {
  ddl_only?: boolean;
}

export interface MigrationApplyRequest {
  name: string;
  sql: string;
  checksum: string;
}

export interface BranchRequest {
  name: string;
}

export interface ClaimRequest {
  email: string;
  password: string;
}

export interface AnonymousRefreshRequest {
  anonymous_id: string;
  anonymous_secret: string;
}

export interface CreateUserRequest {
  username: string;
  password: string;
}

export interface CreateTokenRequest {
  name?: string;
  expires_in_days?: number;
}

// ── Customer response types ────────────────────────────────────

export interface CustomerResponse {
  id: string;
  email: string;
  created_at: string;
  status: string;
}

export interface LoginResponse {
  token: string;
  expires_at: string;
}

export interface AnonymousRegisterResponse {
  token: string;
  expires_at: string;
  is_anonymous: boolean;
  anonymous_id: string;
  anonymous_secret: string;
}

export interface AnonymousRefreshResponse {
  token: string;
  expires_at: string;
}

export interface AnonymousSecretResponse {
  anonymous_id: string;
  anonymous_secret: string;
}

export interface ClaimResponse {
  id: string;
  email: string;
  claimed: boolean;
}

export interface DatabaseResponse {
  id: string;
  name: string;
  state: string;
  region?: string;
  endpoints?: Endpoint[];
  admin_user?: string;
  admin_password?: string;
  created_at: string;
  connection_string?: string;
}

export interface CustomerPasswordResetResponse {
  admin_user: string;
  admin_password: string;
  connection_string: string;
}

export interface TokenResponse {
  id: string;
  name: string;
  created_at: string;
  expires_at?: string;
}

export interface CreateTokenResponse {
  id: string;
  name: string;
  token: string;
  expires_at?: string;
  created_at: string;
}

export interface DumpResponse {
  sql: string;
  object_count: number;
}

export interface SchemaResponse {
  tables: TableMetadata[];
  views: ViewMetadata[];
}

export interface TableMetadata {
  name: string;
  schema: string;
  columns: ColumnMetadata[];
}

export interface ColumnMetadata {
  name: string;
  type: string;
  nullable: boolean;
  default_value?: string;
}

export interface ViewMetadata {
  name: string;
  schema: string;
}

export interface MigrationApplyResponse {
  status: string;
  name: string;
}

export interface MigrationMetadata {
  name: string;
  checksum: string;
  applied_at: string;
  sql_preview: string;
}

export interface Fs9EventEntry {
  id: string;
  type: string;
  path: string;
  timestamp: string;
  user_id?: string;
  size?: number;
  metadata?: Record<string, unknown>;
}

export interface Fs9EventOptions {
  limit?: number;
  offset?: number;
  path?: string;
  type?: string;
}

// ── Union types ────────────────────────────────────────────────

export type TenantState =
  | 'CREATING'
  | 'ACTIVE'
  | 'DISABLING'
  | 'DISABLED'
  | 'CREATE_FAILED';
