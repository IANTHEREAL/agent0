# pg-tikv Admin Portal Redesign

**Status**: Draft  
**Date**: 2025-01-14  
**Related Code**: `scripts/pg_tikv_admin_api.py`, `scripts/pg_tikv_admin.py`

## 1. Background and Current State Analysis

### 1.1 Current Architecture

The pg-tikv Admin Portal is a web management interface for multi-tenant database instances. The current implementation resides in `scripts/pg_tikv_admin_api.py` as a single-file architecture:

```
scripts/pg_tikv_admin_api.py (1734 lines)
├── FastAPI REST API (~400 lines)
├── Pydantic Models (~70 lines)
├── Embedded HTML/CSS/JS (~1200 lines)
└── Business logic from pg_tikv_admin.py
```

### 1.2 System Component Relationships

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Admin Portal (Web UI)                        │
│                    scripts/pg_tikv_admin_api.py                     │
└─────────────────────────────────────────────────────────────────────┘
                                   │
                                   │ HTTP REST API
                                   ▼
┌─────────────────────────────────────────────────────────────────────┐
│                         TenantManager                               │
│                    scripts/pg_tikv_admin.py                         │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  PDClient              │  PgTikvClient                      │   │
│  │  - create_keyspace()   │  - execute_sql()                   │   │
│  │  - list_keyspaces()    │  - create_user()                   │   │
│  │  - get_keyspace()      │  - list_users()                    │   │
│  │  - delete_keyspace()   │  - reset_password()                │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
           │                                    │
           │ HTTP                               │ psql subprocess
           ▼                                    ▼
┌──────────────────┐                ┌──────────────────────────────┐
│    TiKV PD       │                │       pg-tikv Server         │
│  (Keyspace API)  │                │  src/protocol/handler.rs     │
│                  │                │  src/auth/rbac.rs            │
└──────────────────┘                └──────────────────────────────┘
```

### 1.3 Current Issues

| Category | Issue | Severity | Impact |
|----------|-------|----------|--------|
| **Architecture** | Single file with mixed frontend/backend, 1734 lines | High | Hard to maintain, test, collaborate |
| **Architecture** | HTML/CSS/JS embedded as Python string | High | No IDE support, cannot use frontend toolchain |
| **Security** | admin_password passed in URL query params | High | Password logged in server logs/browser history |
| **Security** | API has no authentication, completely open | High | Anyone can access admin interface |
| **Security** | CORS configured as `allow_origins=["*"]` | Medium | Cross-site request vulnerability |
| **Code** | PgTikvClient uses subprocess to call psql | Medium | External tool dependency, limited error handling |
| **Code** | Global singleton manager, hard to test | Medium | Difficult to write unit tests |
| **Code** | Repetitive error handling patterns | Low | Code redundancy |
| **UX** | Limited frontend functionality, no real-time updates | Low | Poor user experience |

### 1.4 Current API Endpoints

| Method | Path | Function |
|--------|------|----------|
| GET | `/api/tenants` | List all tenants |
| POST | `/api/tenants` | Create tenant |
| GET | `/api/tenants/{name}` | Get tenant details |
| DELETE | `/api/tenants/{name}` | Disable tenant |
| GET | `/api/tenants/{tenant}/users` | List users (requires auth) |
| POST | `/api/tenants/{tenant}/users` | Create user (requires auth) |
| DELETE | `/api/tenants/{tenant}/users/{user}` | Delete user (requires auth) |
| POST | `/api/tenants/{tenant}/users/{user}/reset-password` | Reset password (requires auth) |
| GET | `/api/health` | Health check |
| GET | `/api/info` | API information |

### 1.5 Current Security Flow (Problematic)

The current implementation passes tenant credentials via URL query parameters:

```
# Current: Password exposed in URL (BAD)
GET /api/tenants/acme_corp/users?admin_user=admin&admin_password=secret
```

This causes passwords to be:
- Logged in server access logs
- Stored in browser history
- Visible in network monitoring tools
- Cached by proxies

## 2. Design Goals

### 2.1 Primary Goals

1. **Frontend-Backend Separation**: Independent React frontend project, can be developed/deployed separately
2. **Modern UI**: Use shadcn/ui component library for professional admin interface
3. **Security**: Implement API authentication, fix password exposure issues
4. **Maintainability**: Clear code structure, easy to test and extend
5. **Production Ready**: Quality standards suitable for actual deployment

### 2.2 Non-Goals

- Complex RBAC permission system (keep simple admin authentication)
- Real-time data push (WebSocket)
- Multi-language support
- Mobile-responsive design (desktop admin use case)

## 3. Technology Selection

### 3.1 Frontend Stack

| Technology | Choice | Rationale |
|------------|--------|-----------|
| Framework | React 18 + TypeScript | Mature ecosystem, type safety |
| Build Tool | Vite | Fast dev experience, simple config |
| UI Components | shadcn/ui | Customizable, beautiful design, no runtime dependency |
| Styling | Tailwind CSS | Required by shadcn/ui, utility-first |
| State Management | TanStack Query | Server state management, caching/retry |
| Tables | TanStack Table | Recommended by shadcn/ui, feature complete |
| Routing | React Router v6 | Lightweight, sufficient for simple scenarios |
| Forms | React Hook Form + Zod | Type-safe form validation |

### 3.2 Backend Stack

| Technology | Choice | Rationale |
|------------|--------|-----------|
| Framework | FastAPI (keep) | Existing choice, good performance, auto docs |
| Authentication | JWT (simple implementation) | Stateless, easy to implement |
| Configuration | pydantic-settings | Type-safe config management |

## 4. System Architecture

### 4.1 New Architecture Overview

```
pg-tikv/
├── scripts/
│   ├── pg_tikv_admin.py          # CLI tool (keep as-is)
│   └── pg_tikv_admin_api.py      # REST API (refactor)
│
└── admin-ui/                      # NEW: Frontend project
    ├── package.json
    ├── vite.config.ts
    ├── tailwind.config.js
    ├── tsconfig.json
    ├── index.html
    └── src/
        ├── main.tsx
        ├── App.tsx
        ├── api/                   # API client
        │   ├── client.ts
        │   ├── tenants.ts
        │   └── users.ts
        ├── components/
        │   ├── ui/               # shadcn/ui components
        │   ├── layout/           # Layout components
        │   ├── tenants/          # Tenant-related components
        │   └── users/            # User-related components
        ├── hooks/                 # Custom hooks
        ├── lib/                   # Utility functions
        ├── pages/                 # Page components
        └── types/                 # TypeScript types
```

### 4.2 Deployment Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Browser                                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ HTTPS
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Reverse Proxy (Nginx)                         │
│  ┌─────────────────────────┐  ┌─────────────────────────────┐   │
│  │  /                      │  │  /api/*                      │   │
│  │  Static Files (SPA)     │  │  FastAPI Backend             │   │
│  └─────────────────────────┘  └─────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
                                            │
                    ┌───────────────────────┴───────────────────┐
                    ▼                                           ▼
          ┌─────────────────┐                       ┌─────────────────┐
          │   TiKV PD       │                       │   pg-tikv       │
          │   (Keyspaces)   │                       │   (SQL)         │
          └─────────────────┘                       └─────────────────┘
```

### 4.3 Development Mode

During development, the frontend runs on Vite dev server (port 5173) and proxies API requests to the backend (port 8080):

```
┌──────────────────┐         ┌──────────────────┐
│  Vite Dev Server │  proxy  │  FastAPI Server  │
│  localhost:5173  │ ──────► │  localhost:8080  │
└──────────────────┘         └──────────────────┘
```

## 5. API Design

### 5.1 Authentication Mechanism

Use simple JWT authentication. The API server maintains an admin password (via environment variable or config file).

**Authentication Flow:**

```
1. POST /api/auth/login { "password": "admin_secret" }
   → { "token": "eyJhbGciOiJIUzI1NiIs...", "expires_at": "..." }

2. Subsequent requests include header:
   Authorization: Bearer <token>

3. Token validity: 24 hours (configurable)
```

**JWT Payload:**

```json
{
  "sub": "admin",
  "iat": 1704067200,
  "exp": 1704153600
}
```

### 5.2 Tenant Credential Management

To avoid repeatedly entering tenant database credentials, introduce a "Tenant Session" concept:

```
1. User authenticates to tenant via POST /api/tenants/{name}/connect
   Request Body: { "admin_user": "admin", "admin_password": "secret" }
   
2. Server validates credentials against pg-tikv
   
3. On success, returns session_id (stored in server memory, expires in 1 hour)
   Response: { "session_id": "ts_abc123", "expires_at": "..." }

4. Subsequent user management requests include header:
   X-Tenant-Session: ts_abc123
```

This approach:
- Keeps passwords out of URLs
- Avoids sending passwords on every request
- Allows session timeout for security
- Simplifies frontend state management

### 5.3 API Endpoints

#### 5.3.1 Authentication

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| POST | `/api/auth/login` | Login to get token | None |
| POST | `/api/auth/refresh` | Refresh token | Bearer |
| GET | `/api/auth/me` | Get current user info | Bearer |

#### 5.3.2 Tenant Management

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| GET | `/api/tenants` | List all tenants | Bearer |
| POST | `/api/tenants` | Create tenant | Bearer |
| GET | `/api/tenants/{name}` | Get tenant details | Bearer |
| DELETE | `/api/tenants/{name}` | Disable tenant | Bearer |

#### 5.3.3 Tenant Connection & User Management

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| POST | `/api/tenants/{name}/connect` | Connect to tenant (validates credentials) | Bearer |
| GET | `/api/tenants/{name}/users` | List users | Bearer + X-Tenant-Session |
| POST | `/api/tenants/{name}/users` | Create user | Bearer + X-Tenant-Session |
| DELETE | `/api/tenants/{name}/users/{user}` | Delete user | Bearer + X-Tenant-Session |
| POST | `/api/tenants/{name}/users/{user}/password` | Reset password | Bearer + X-Tenant-Session |

#### 5.3.4 System

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| GET | `/api/health` | Health check | None |
| GET | `/api/info` | API information | None |

### 5.4 Request/Response Formats

#### Login Request

```http
POST /api/auth/login
Content-Type: application/json

{
  "password": "admin_secret"
}
```

#### Login Response

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "token": "eyJhbGciOiJIUzI1NiIs...",
  "expires_at": "2025-01-15T20:00:00Z"
}
```

#### Create Tenant Request

```http
POST /api/tenants
Authorization: Bearer <token>
Content-Type: application/json

{
  "name": "acme_corp",
  "admin_user": "admin",
  "admin_password": "optional_password"
}
```

#### Create Tenant Response

```http
HTTP/1.1 201 Created
Content-Type: application/json

{
  "name": "acme_corp",
  "admin_user": "admin",
  "admin_password": "generated_or_provided",
  "connection_string": "postgresql://acme_corp.admin:xxx@host:5433/postgres",
  "created_at": "2025-01-14T20:00:00Z"
}
```

#### Connect to Tenant Request

```http
POST /api/tenants/acme_corp/connect
Authorization: Bearer <token>
Content-Type: application/json

{
  "admin_user": "admin",
  "admin_password": "secret"
}
```

#### Connect to Tenant Response

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "session_id": "ts_abc123",
  "expires_at": "2025-01-14T21:00:00Z"
}
```

#### List Users Request

```http
GET /api/tenants/acme_corp/users
Authorization: Bearer <token>
X-Tenant-Session: ts_abc123
```

#### Error Response Format

```http
HTTP/1.1 400 Bad Request
Content-Type: application/json

{
  "error": "validation_error",
  "message": "Tenant name must be 3-64 lowercase alphanumeric characters",
  "details": {
    "field": "name",
    "constraint": "pattern"
  }
}
```

## 6. Frontend Design

### 6.1 Page Structure

```
/                     → Redirect to /tenants
/login                → Login page
/tenants              → Tenant list page
/tenants/:name        → Tenant detail page (includes user management)
```

### 6.2 Component Hierarchy

```
App
├── QueryClientProvider
│   └── AuthProvider
│       └── Router
│           ├── LoginPage
│           └── ProtectedRoute
│               └── Layout
│                   ├── Sidebar
│                   ├── Header
│                   └── Outlet
│                       ├── TenantsPage
│                       │   ├── TenantTable
│                       │   ├── CreateTenantDialog
│                       │   └── DeleteTenantDialog
│                       └── TenantDetailPage
│                           ├── TenantInfo
│                           ├── ConnectDialog
│                           ├── UsersTable
│                           ├── CreateUserDialog
│                           └── ResetPasswordDialog
```

### 6.3 Core Components

#### 6.3.1 TenantTable

Using TanStack Table + shadcn/ui Table component:

```tsx
// admin-ui/src/components/tenants/TenantTable.tsx
import { ColumnDef } from "@tanstack/react-table"
import { DataTable } from "@/components/ui/data-table"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Link } from "react-router-dom"

interface Tenant {
  name: string
  state: "ENABLED" | "DISABLED"
}

const columns: ColumnDef<Tenant>[] = [
  {
    accessorKey: "name",
    header: "Name",
    cell: ({ row }) => (
      <Link 
        to={`/tenants/${row.original.name}`}
        className="font-medium hover:underline"
      >
        {row.original.name}
      </Link>
    ),
  },
  {
    accessorKey: "state",
    header: "Status",
    cell: ({ row }) => (
      <Badge variant={row.original.state === "ENABLED" ? "default" : "destructive"}>
        {row.original.state}
      </Badge>
    ),
  },
  {
    id: "connection",
    header: "Connection",
    cell: ({ row }) => (
      <code className="text-sm">{row.original.name}.&lt;user&gt;</code>
    ),
  },
  {
    id: "actions",
    header: "",
    cell: ({ row }) => <TenantActions tenant={row.original} />,
  },
]

export function TenantTable() {
  const { data: tenants, isLoading } = useTenants()
  
  if (isLoading) return <TableSkeleton />
  
  return <DataTable columns={columns} data={tenants ?? []} />
}
```

#### 6.3.2 CreateTenantDialog

Using shadcn/ui Dialog + React Hook Form + Zod:

```tsx
// admin-ui/src/components/tenants/CreateTenantDialog.tsx
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import { z } from "zod"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  Form,
  FormControl,
  FormDescription,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Button } from "@/components/ui/button"
import { useCreateTenant } from "@/api/tenants"
import { useToast } from "@/components/ui/use-toast"

const schema = z.object({
  name: z.string()
    .min(3, "Name must be at least 3 characters")
    .max(64, "Name must be at most 64 characters")
    .regex(/^[a-z0-9_]+$/, "Only lowercase letters, numbers, and underscores"),
  admin_user: z.string().default("admin"),
  admin_password: z.string().optional(),
})

type FormData = z.infer<typeof schema>

interface Props {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CreateTenantDialog({ open, onOpenChange }: Props) {
  const { toast } = useToast()
  const form = useForm<FormData>({
    resolver: zodResolver(schema),
    defaultValues: { name: "", admin_user: "admin", admin_password: "" },
  })

  const mutation = useCreateTenant()

  const onSubmit = (data: FormData) => {
    mutation.mutate(data, {
      onSuccess: (result) => {
        toast({
          title: "Tenant Created",
          description: (
            <div className="mt-2 space-y-2">
              <p>Password: <code>{result.admin_password}</code></p>
              <p className="text-sm text-muted-foreground">
                Save this password - it won't be shown again.
              </p>
            </div>
          ),
        })
        form.reset()
        onOpenChange(false)
      },
      onError: (error) => {
        toast({
          title: "Error",
          description: error.message,
          variant: "destructive",
        })
      },
    })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create Tenant</DialogTitle>
          <DialogDescription>
            Create a new isolated database tenant with its own keyspace.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
            <FormField
              control={form.control}
              name="name"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Tenant Name</FormLabel>
                  <FormControl>
                    <Input placeholder="acme_corp" {...field} />
                  </FormControl>
                  <FormDescription>
                    Lowercase letters, numbers, and underscores (3-64 chars)
                  </FormDescription>
                  <FormMessage />
                </FormItem>
              )}
            />
            <div className="grid grid-cols-2 gap-4">
              <FormField
                control={form.control}
                name="admin_user"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Admin User</FormLabel>
                    <FormControl>
                      <Input {...field} />
                    </FormControl>
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name="admin_password"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Password</FormLabel>
                    <FormControl>
                      <Input placeholder="Auto-generate" {...field} />
                    </FormControl>
                  </FormItem>
                )}
              />
            </div>
            <div className="flex justify-end gap-2">
              <Button 
                type="button" 
                variant="outline" 
                onClick={() => onOpenChange(false)}
              >
                Cancel
              </Button>
              <Button type="submit" disabled={mutation.isPending}>
                {mutation.isPending ? "Creating..." : "Create Tenant"}
              </Button>
            </div>
          </form>
        </Form>
      </DialogContent>
    </Dialog>
  )
}
```

### 6.4 API Client

Using TanStack Query to manage server state:

```tsx
// admin-ui/src/api/client.ts
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
    ...options.headers as Record<string, string>,
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
    throw new ApiError(response.status, error.message, error.details)
  }

  if (response.status === 204) {
    return undefined as T
  }
  
  return response.json()
}
```

```tsx
// admin-ui/src/api/tenants.ts
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query"
import { apiRequest } from "./client"

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
      apiRequest(`/tenants/${name}`, { method: "DELETE" }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["tenants"] })
    },
  })
}
```

### 6.5 Authentication Hook

```tsx
// admin-ui/src/hooks/useAuth.tsx
import { createContext, useContext, useState, useEffect, ReactNode } from "react"
import { useNavigate } from "react-router-dom"
import { apiRequest, ApiError } from "@/api/client"

interface AuthContextType {
  isAuthenticated: boolean
  isLoading: boolean
  login: (password: string) => Promise<void>
  logout: () => void
}

const AuthContext = createContext<AuthContextType | null>(null)

export function AuthProvider({ children }: { children: ReactNode }) {
  const [isAuthenticated, setIsAuthenticated] = useState(false)
  const [isLoading, setIsLoading] = useState(true)
  const navigate = useNavigate()

  useEffect(() => {
    const token = localStorage.getItem("auth_token")
    if (token) {
      // Validate token by calling /api/auth/me
      apiRequest("/auth/me")
        .then(() => setIsAuthenticated(true))
        .catch(() => {
          localStorage.removeItem("auth_token")
          setIsAuthenticated(false)
        })
        .finally(() => setIsLoading(false))
    } else {
      setIsLoading(false)
    }
  }, [])

  const login = async (password: string) => {
    const response = await apiRequest<{ token: string }>("/auth/login", {
      method: "POST",
      body: JSON.stringify({ password }),
    })
    localStorage.setItem("auth_token", response.token)
    setIsAuthenticated(true)
    navigate("/tenants")
  }

  const logout = () => {
    localStorage.removeItem("auth_token")
    sessionStorage.removeItem("tenant_session")
    setIsAuthenticated(false)
    navigate("/login")
  }

  return (
    <AuthContext.Provider value={{ isAuthenticated, isLoading, login, logout }}>
      {children}
    </AuthContext.Provider>
  )
}

export function useAuth() {
  const context = useContext(AuthContext)
  if (!context) {
    throw new Error("useAuth must be used within AuthProvider")
  }
  return context
}
```

### 6.6 UI Mockups (ASCII)

#### Login Page

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│                                                                 │
│                     ┌─────────────────────┐                     │
│                     │      [Logo]         │                     │
│                     │      pg-tikv        │                     │
│                     │   Admin Console     │                     │
│                     │                     │                     │
│                     │  ┌───────────────┐  │                     │
│                     │  │   Password    │  │                     │
│                     │  └───────────────┘  │                     │
│                     │                     │                     │
│                     │  [    Login    ]    │                     │
│                     │                     │                     │
│                     └─────────────────────┘                     │
│                                                                 │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

#### Tenants List Page

```
┌─────────────────────────────────────────────────────────────────┐
│  pg-tikv Admin                               [●] Connected  [⚙]│
├─────────┬───────────────────────────────────────────────────────┤
│         │                                                       │
│ Tenants │  Tenants                        [ + New Tenant ]      │
│         │  Manage your multi-tenant database instances          │
│         │  ─────────────────────────────────────────────────    │
│         │                                                       │
│         │  ┌─────────────────────────────────────────────────┐  │
│         │  │ NAME           │ STATUS   │ CONNECTION │ ACTIONS│  │
│         │  ├────────────────┼──────────┼────────────┼────────┤  │
│         │  │ acme_corp      │ [ENABLED]│ acme_corp. │  [···] │  │
│         │  │ beta_inc       │ [ENABLED]│ beta_inc.  │  [···] │  │
│         │  │ test_tenant    │[DISABLED]│ test_tena. │  [···] │  │
│         │  └─────────────────────────────────────────────────┘  │
│         │                                                       │
│         │  Showing 3 tenants                                    │
│         │                                                       │
└─────────┴───────────────────────────────────────────────────────┘
```

#### Tenant Detail Page

```
┌─────────────────────────────────────────────────────────────────┐
│  pg-tikv Admin                               [●] Connected  [⚙]│
├─────────┬───────────────────────────────────────────────────────┤
│         │                                                       │
│ Tenants │  ← Back to Tenants                                    │
│         │                                                       │
│         │  acme_corp                              [●] ENABLED   │
│         │  ─────────────────────────────────────────────────    │
│         │                                                       │
│         │  Connection Info                                      │
│         │  ┌─────────────────────────────────────────────────┐  │
│         │  │ Host: 127.0.0.1        Port: 5433               │  │
│         │  │ User format: acme_corp.<username>               │  │
│         │  │                                                 │  │
│         │  │ psql -h 127.0.0.1 -p 5433 -U acme_corp.admin    │  │
│         │  └─────────────────────────────────────────────────┘  │
│         │                                                       │
│         │  Users                               [ + New User ]   │
│         │  ┌─────────────────────────────────────────────────┐  │
│         │  │ Connect to manage users                         │  │
│         │  │                                                 │  │
│         │  │ Admin User: [admin    ] Password: [••••••••]    │  │
│         │  │                                  [ Connect ]     │  │
│         │  └─────────────────────────────────────────────────┘  │
│         │                                                       │
│         │  ┌─────────────────────────────────────────────────┐  │
│         │  │ NAME     │ SUPERUSER │ LOGIN │ ACTIONS          │  │
│         │  ├──────────┼───────────┼───────┼──────────────────┤  │
│         │  │ admin    │ ✓         │ ✓     │ [Reset] [Delete] │  │
│         │  │ developer│ -         │ ✓     │ [Reset] [Delete] │  │
│         │  └─────────────────────────────────────────────────┘  │
│         │                                                       │
└─────────┴───────────────────────────────────────────────────────┘
```

## 7. Backend Refactoring

### 7.1 New Code Structure

```python
# scripts/pg_tikv_admin_api.py (refactored)

"""
pg-tikv Admin REST API Server

RESTful API for pg-tikv multi-tenant administration.
"""

from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from functools import lru_cache
from typing import Optional, Dict
import os
import secrets

from fastapi import FastAPI, HTTPException, Depends, Header, status
from fastapi.middleware.cors import CORSMiddleware
from fastapi.security import HTTPBearer, HTTPAuthorizationCredentials
from pydantic import BaseModel, Field
from pydantic_settings import BaseSettings
import jwt
import uvicorn

from pg_tikv_admin import TenantManager, generate_password, UserInfo

# ============================================================================
# Configuration
# ============================================================================

class Settings(BaseSettings):
    """Application settings loaded from environment variables."""
    pd_endpoints: str = "127.0.0.1:2379"
    pg_host: str = "127.0.0.1"
    pg_port: int = 5433
    api_port: int = 8080
    admin_password: str = "admin"  # MUST change in production
    jwt_secret: str = Field(default_factory=lambda: secrets.token_hex(32))
    jwt_expiry_hours: int = 24
    cors_origins: list[str] = ["http://localhost:5173"]  # Vite dev server
    tenant_session_expiry_hours: int = 1

    class Config:
        env_prefix = "PGTIKV_"


@lru_cache()
def get_settings() -> Settings:
    return Settings()


# ============================================================================
# Tenant Session Storage
# ============================================================================

@dataclass
class TenantSession:
    """Stores validated tenant credentials for user management operations."""
    tenant_name: str
    admin_user: str
    admin_password: str
    expires_at: datetime


# In-memory storage. Production should use Redis.
_tenant_sessions: Dict[str, TenantSession] = {}


def cleanup_expired_sessions():
    """Remove expired tenant sessions."""
    now = datetime.now(timezone.utc)
    expired = [k for k, v in _tenant_sessions.items() if v.expires_at < now]
    for k in expired:
        del _tenant_sessions[k]


# ============================================================================
# Dependencies
# ============================================================================

def get_manager(settings: Settings = Depends(get_settings)) -> TenantManager:
    """Get TenantManager instance."""
    return TenantManager(settings.pd_endpoints, settings.pg_host, settings.pg_port)


security = HTTPBearer(auto_error=False)


async def get_current_user(
    credentials: Optional[HTTPAuthorizationCredentials] = Depends(security),
    settings: Settings = Depends(get_settings),
) -> str:
    """Validate JWT token and return username."""
    if not credentials:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Authentication required",
            headers={"WWW-Authenticate": "Bearer"},
        )
    try:
        payload = jwt.decode(
            credentials.credentials,
            settings.jwt_secret,
            algorithms=["HS256"]
        )
        return payload["sub"]
    except jwt.ExpiredSignatureError:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Token has expired",
        )
    except jwt.InvalidTokenError:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid token",
        )


async def get_tenant_session(
    tenant_name: str,
    x_tenant_session: Optional[str] = Header(None, alias="X-Tenant-Session"),
) -> TenantSession:
    """Validate tenant session for user management operations."""
    if not x_tenant_session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Tenant session required. Use POST /api/tenants/{name}/connect first.",
        )
    
    cleanup_expired_sessions()
    
    session = _tenant_sessions.get(x_tenant_session)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired tenant session",
        )
    if session.tenant_name != tenant_name:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Session does not match tenant",
        )
    
    return session


# ============================================================================
# Pydantic Models
# ============================================================================

class LoginRequest(BaseModel):
    password: str


class LoginResponse(BaseModel):
    token: str
    expires_at: datetime


class TenantCreate(BaseModel):
    name: str = Field(
        ...,
        min_length=3,
        max_length=64,
        pattern=r'^[a-z0-9_]+$',
        description="Tenant name (lowercase alphanumeric with underscores)"
    )
    admin_user: str = Field(default="admin", description="Admin username")
    admin_password: Optional[str] = Field(
        default=None,
        description="Admin password (auto-generated if not provided)"
    )


class TenantResponse(BaseModel):
    name: str
    state: str
    host: Optional[str] = None
    port: Optional[int] = None


class TenantCreateResponse(BaseModel):
    name: str
    admin_user: str
    admin_password: str
    connection_string: str
    created_at: datetime


class TenantConnectRequest(BaseModel):
    admin_user: str = Field(default="admin", description="Admin username")
    admin_password: str = Field(..., description="Admin password")


class TenantConnectResponse(BaseModel):
    session_id: str
    expires_at: datetime


class UserCreate(BaseModel):
    username: str = Field(..., min_length=1, max_length=64)
    password: Optional[str] = Field(
        default=None,
        description="Password (auto-generated if not provided)"
    )
    superuser: bool = Field(default=False, description="Create as superuser")


class UserResponse(BaseModel):
    name: str
    is_superuser: bool
    can_login: bool
    can_create_db: bool
    can_create_role: bool


class UserCreateResponse(BaseModel):
    username: str
    password: str
    connection: str


class PasswordResetResponse(BaseModel):
    username: str
    password: str


class MessageResponse(BaseModel):
    message: str


class HealthResponse(BaseModel):
    status: str
    pd_healthy: bool


# ============================================================================
# Application Setup
# ============================================================================

@asynccontextmanager
async def lifespan(app: FastAPI):
    """Application lifespan handler."""
    # Startup
    yield
    # Shutdown: cleanup sessions
    _tenant_sessions.clear()


app = FastAPI(
    title="pg-tikv Admin API",
    description="RESTful API for pg-tikv multi-tenant administration",
    version="2.0.0",
    docs_url="/api/docs",
    redoc_url="/api/redoc",
    openapi_url="/api/openapi.json",
    lifespan=lifespan,
)


@app.on_event("startup")
async def setup_middleware():
    settings = get_settings()
    app.add_middleware(
        CORSMiddleware,
        allow_origins=settings.cors_origins,
        allow_credentials=True,
        allow_methods=["GET", "POST", "DELETE"],
        allow_headers=["Authorization", "X-Tenant-Session", "Content-Type"],
    )


# ============================================================================
# Auth Endpoints
# ============================================================================

@app.post(
    "/api/auth/login",
    response_model=LoginResponse,
    tags=["Authentication"],
    summary="Login to get JWT token",
)
async def login(
    request: LoginRequest,
    settings: Settings = Depends(get_settings),
):
    """
    Authenticate with admin password and receive a JWT token.
    
    The token should be included in subsequent requests as:
    `Authorization: Bearer <token>`
    """
    if request.password != settings.admin_password:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid password",
        )
    
    expires_at = datetime.now(timezone.utc) + timedelta(hours=settings.jwt_expiry_hours)
    token = jwt.encode(
        {"sub": "admin", "iat": datetime.now(timezone.utc), "exp": expires_at},
        settings.jwt_secret,
        algorithm="HS256"
    )
    return LoginResponse(token=token, expires_at=expires_at)


@app.get(
    "/api/auth/me",
    tags=["Authentication"],
    summary="Get current user info",
)
async def get_me(user: str = Depends(get_current_user)):
    """Returns information about the currently authenticated user."""
    return {"user": user}


# ============================================================================
# Tenant Endpoints
# ============================================================================

@app.get(
    "/api/tenants",
    response_model=list[TenantResponse],
    tags=["Tenants"],
    summary="List all tenants",
)
async def list_tenants(
    _: str = Depends(get_current_user),
    manager: TenantManager = Depends(get_manager),
):
    """List all tenants (TiKV keyspaces)."""
    tenants = manager.list_tenants()
    return [TenantResponse(name=t["name"], state=t["state"]) for t in tenants]


@app.post(
    "/api/tenants",
    response_model=TenantCreateResponse,
    status_code=status.HTTP_201_CREATED,
    tags=["Tenants"],
    summary="Create a new tenant",
)
async def create_tenant(
    request: TenantCreate,
    _: str = Depends(get_current_user),
    manager: TenantManager = Depends(get_manager),
    settings: Settings = Depends(get_settings),
):
    """
    Create a new tenant with its own isolated keyspace.
    
    Returns the admin credentials. Save the password - it won't be shown again.
    """
    # Check if already exists
    if manager.get_tenant(request.name):
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Tenant '{request.name}' already exists",
        )
    
    # Create keyspace
    if not manager.pd.create_keyspace(request.name):
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create keyspace in TiKV",
        )
    
    password = request.admin_password or generate_password()
    
    return TenantCreateResponse(
        name=request.name,
        admin_user=request.admin_user,
        admin_password=password,
        connection_string=(
            f"postgresql://{request.name}.{request.admin_user}:{password}"
            f"@{settings.pg_host}:{settings.pg_port}/postgres"
        ),
        created_at=datetime.now(timezone.utc),
    )


@app.get(
    "/api/tenants/{name}",
    response_model=TenantResponse,
    tags=["Tenants"],
    summary="Get tenant details",
)
async def get_tenant(
    name: str,
    _: str = Depends(get_current_user),
    manager: TenantManager = Depends(get_manager),
    settings: Settings = Depends(get_settings),
):
    """Get details for a specific tenant."""
    tenant = manager.get_tenant(name)
    if not tenant:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    return TenantResponse(
        name=tenant["name"],
        state="ENABLED",
        host=settings.pg_host,
        port=settings.pg_port,
    )


@app.delete(
    "/api/tenants/{name}",
    response_model=MessageResponse,
    tags=["Tenants"],
    summary="Disable a tenant",
)
async def delete_tenant(
    name: str,
    _: str = Depends(get_current_user),
    manager: TenantManager = Depends(get_manager),
):
    """
    Disable a tenant.
    
    Note: TiKV keyspaces cannot be fully deleted, only disabled.
    The data remains but becomes inaccessible.
    """
    if not manager.get_tenant(name):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    if not manager.delete_tenant(name, force=True):
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to disable tenant",
        )
    
    return MessageResponse(message=f"Tenant '{name}' disabled")


# ============================================================================
# Tenant Connection & User Endpoints
# ============================================================================

@app.post(
    "/api/tenants/{name}/connect",
    response_model=TenantConnectResponse,
    tags=["Users"],
    summary="Connect to tenant for user management",
)
async def connect_tenant(
    name: str,
    request: TenantConnectRequest,
    _: str = Depends(get_current_user),
    manager: TenantManager = Depends(get_manager),
    settings: Settings = Depends(get_settings),
):
    """
    Validate tenant credentials and create a session for user management.
    
    Returns a session_id that should be included in subsequent user
    management requests as the `X-Tenant-Session` header.
    """
    # Validate tenant exists
    if not manager.get_tenant(name):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    # Validate credentials
    if not manager.pg.test_connection(name, request.admin_user, request.admin_password):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid tenant credentials",
        )
    
    # Create session
    session_id = f"ts_{secrets.token_hex(16)}"
    expires_at = datetime.now(timezone.utc) + timedelta(
        hours=settings.tenant_session_expiry_hours
    )
    
    _tenant_sessions[session_id] = TenantSession(
        tenant_name=name,
        admin_user=request.admin_user,
        admin_password=request.admin_password,
        expires_at=expires_at,
    )
    
    return TenantConnectResponse(session_id=session_id, expires_at=expires_at)


@app.get(
    "/api/tenants/{name}/users",
    response_model=list[UserResponse],
    tags=["Users"],
    summary="List users in tenant",
)
async def list_users(
    name: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    manager: TenantManager = Depends(get_manager),
):
    """List all users in the tenant. Requires tenant session."""
    users = manager.list_users(name, session.admin_user, session.admin_password)
    return [
        UserResponse(
            name=u.name,
            is_superuser=u.is_superuser,
            can_login=u.can_login,
            can_create_db=u.can_create_db,
            can_create_role=u.can_create_role,
        )
        for u in users
    ]


@app.post(
    "/api/tenants/{name}/users",
    response_model=UserCreateResponse,
    status_code=status.HTTP_201_CREATED,
    tags=["Users"],
    summary="Create user in tenant",
)
async def create_user(
    name: str,
    request: UserCreate,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    manager: TenantManager = Depends(get_manager),
    settings: Settings = Depends(get_settings),
):
    """Create a new user in the tenant. Requires tenant session."""
    password = request.password or generate_password()
    
    success = manager.pg.create_user(
        name,
        session.admin_user,
        session.admin_password,
        request.username,
        password,
        request.superuser,
    )
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create user",
        )
    
    return UserCreateResponse(
        username=request.username,
        password=password,
        connection=f"psql -h {settings.pg_host} -p {settings.pg_port} -U {name}.{request.username}",
    )


@app.delete(
    "/api/tenants/{name}/users/{username}",
    response_model=MessageResponse,
    tags=["Users"],
    summary="Delete user from tenant",
)
async def delete_user(
    name: str,
    username: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    manager: TenantManager = Depends(get_manager),
):
    """Delete a user from the tenant. Requires tenant session."""
    success = manager.pg.drop_user(name, session.admin_user, session.admin_password, username)
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to delete user",
        )
    
    return MessageResponse(message=f"User '{username}' deleted")


@app.post(
    "/api/tenants/{name}/users/{username}/password",
    response_model=PasswordResetResponse,
    tags=["Users"],
    summary="Reset user password",
)
async def reset_password(
    name: str,
    username: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    manager: TenantManager = Depends(get_manager),
):
    """Reset a user's password. Requires tenant session."""
    new_password = generate_password()
    
    success = manager.pg.reset_password(
        name,
        session.admin_user,
        session.admin_password,
        username,
        new_password,
    )
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to reset password",
        )
    
    return PasswordResetResponse(username=username, password=new_password)


# ============================================================================
# System Endpoints
# ============================================================================

@app.get(
    "/api/health",
    response_model=HealthResponse,
    tags=["System"],
    summary="Health check",
)
async def health_check(manager: TenantManager = Depends(get_manager)):
    """Check API and PD health status."""
    pd_healthy = bool(manager.pd.list_keyspaces())
    return HealthResponse(
        status="healthy" if pd_healthy else "degraded",
        pd_healthy=pd_healthy,
    )


@app.get(
    "/api/info",
    tags=["System"],
    summary="API information",
)
async def api_info():
    """Get API version and information."""
    return {
        "name": "pg-tikv Admin API",
        "version": "2.0.0",
        "docs": "/api/docs",
    }


# ============================================================================
# Main
# ============================================================================

def main():
    """Run the API server."""
    settings = get_settings()
    
    print(f"""
╔═══════════════════════════════════════════════════════════════╗
║              pg-tikv Admin API Server v2.0                    ║
╠═══════════════════════════════════════════════════════════════╣
║  API URL:     http://0.0.0.0:{settings.api_port}/api
║  API Docs:    http://0.0.0.0:{settings.api_port}/api/docs
║                                                               
║  PD Endpoints: {settings.pd_endpoints}
║  PG Host:      {settings.pg_host}
║  PG Port:      {settings.pg_port}
╚═══════════════════════════════════════════════════════════════╝
""")
    
    uvicorn.run(app, host="0.0.0.0", port=settings.api_port)


if __name__ == "__main__":
    main()
```

### 7.2 Key Improvements

| Area | Before | After |
|------|--------|-------|
| **Authentication** | None | JWT Bearer token |
| **Password Security** | In URL query params | In request body + tenant session |
| **Configuration** | Scattered env vars | Centralized pydantic-settings |
| **Dependencies** | Global singleton | FastAPI Depends injection |
| **Error Handling** | Repetitive try/except | HTTPException with proper codes |
| **API Docs** | Basic | Full OpenAPI with descriptions |
| **CORS** | `allow_origins=["*"]` | Configurable whitelist |

## 8. Implementation Plan

### Phase 1: Backend Refactoring (1-2 days)

1. Refactor `pg_tikv_admin_api.py`
   - Add JWT authentication
   - Implement Tenant Session mechanism
   - Fix CORS configuration
   - Add configuration management
   - Remove embedded HTML

2. Add API tests
   - Use pytest + httpx for endpoint testing
   - Test authentication flow
   - Test error handling

### Phase 2: Frontend Project Initialization (1 day)

1. Create `admin-ui/` project
   ```bash
   npm create vite@latest admin-ui -- --template react-ts
   cd admin-ui
   npx shadcn@latest init
   ```

2. Install dependencies
   ```bash
   npm install @tanstack/react-query @tanstack/react-table react-router-dom
   npm install react-hook-form @hookform/resolvers zod
   ```

3. Add shadcn/ui components
   ```bash
   npx shadcn@latest add button card dialog form input table badge toast
   ```

4. Configure Vite proxy for development
   ```ts
   // vite.config.ts
   export default defineConfig({
     server: {
       proxy: {
         '/api': 'http://localhost:8080'
       }
     }
   })
   ```

### Phase 3: Frontend Core Features (2-3 days)

1. Implement authentication flow
   - LoginPage
   - AuthProvider
   - ProtectedRoute

2. Implement tenant management
   - TenantsPage
   - TenantTable
   - CreateTenantDialog
   - DeleteTenantDialog

3. Implement user management
   - TenantDetailPage
   - ConnectDialog
   - UsersTable
   - CreateUserDialog
   - ResetPasswordDialog

### Phase 4: Integration & Testing (1 day)

1. Frontend-backend integration testing
2. Fix discovered issues
3. Update documentation

### Phase 5: Deployment Configuration (Optional)

1. Add Docker support
2. Add Nginx configuration example
3. Add production deployment documentation

## 9. File Change Summary

### New Files

```
admin-ui/
├── package.json
├── vite.config.ts
├── tailwind.config.js
├── tsconfig.json
├── index.html
├── .env.example
└── src/
    ├── main.tsx
    ├── App.tsx
    ├── vite-env.d.ts
    ├── index.css
    ├── api/
    │   ├── client.ts
    │   ├── auth.ts
    │   ├── tenants.ts
    │   └── users.ts
    ├── components/
    │   ├── ui/                    # shadcn/ui components (generated)
    │   │   ├── button.tsx
    │   │   ├── card.tsx
    │   │   ├── dialog.tsx
    │   │   ├── form.tsx
    │   │   ├── input.tsx
    │   │   ├── table.tsx
    │   │   ├── badge.tsx
    │   │   ├── toast.tsx
    │   │   └── ...
    │   ├── layout/
    │   │   ├── Layout.tsx
    │   │   ├── Sidebar.tsx
    │   │   └── Header.tsx
    │   ├── tenants/
    │   │   ├── TenantTable.tsx
    │   │   ├── TenantActions.tsx
    │   │   ├── CreateTenantDialog.tsx
    │   │   └── DeleteTenantDialog.tsx
    │   └── users/
    │       ├── UsersTable.tsx
    │       ├── ConnectDialog.tsx
    │       ├── CreateUserDialog.tsx
    │       └── ResetPasswordDialog.tsx
    ├── hooks/
    │   ├── useAuth.tsx
    │   └── useTenantSession.tsx
    ├── lib/
    │   └── utils.ts
    ├── pages/
    │   ├── LoginPage.tsx
    │   ├── TenantsPage.tsx
    │   └── TenantDetailPage.tsx
    └── types/
        └── index.ts
```

### Modified Files

```
scripts/pg_tikv_admin_api.py     # Refactored (remove embedded HTML, add auth)
```

### Removed Content

```
scripts/pg_tikv_admin_api.py: ADMIN_HTML variable (~1200 lines)
```

## 10. Testing Strategy

### 10.1 Backend Tests

```python
# tests/test_admin_api.py
import pytest
from fastapi.testclient import TestClient
from unittest.mock import patch, MagicMock

# Import after setting test env vars
import os
os.environ["PGTIKV_ADMIN_PASSWORD"] = "test_password"
os.environ["PGTIKV_JWT_SECRET"] = "test_secret"

from scripts.pg_tikv_admin_api import app, get_settings, get_manager

client = TestClient(app)


@pytest.fixture
def auth_token():
    """Get valid auth token for tests."""
    response = client.post("/api/auth/login", json={"password": "test_password"})
    return response.json()["token"]


@pytest.fixture
def auth_headers(auth_token):
    """Get auth headers for authenticated requests."""
    return {"Authorization": f"Bearer {auth_token}"}


class TestAuth:
    def test_login_success(self):
        response = client.post("/api/auth/login", json={"password": "test_password"})
        assert response.status_code == 200
        assert "token" in response.json()
        assert "expires_at" in response.json()

    def test_login_failure(self):
        response = client.post("/api/auth/login", json={"password": "wrong"})
        assert response.status_code == 401

    def test_me_unauthorized(self):
        response = client.get("/api/auth/me")
        assert response.status_code == 401

    def test_me_authorized(self, auth_headers):
        response = client.get("/api/auth/me", headers=auth_headers)
        assert response.status_code == 200
        assert response.json()["user"] == "admin"


class TestTenants:
    def test_list_tenants_unauthorized(self):
        response = client.get("/api/tenants")
        assert response.status_code == 401

    @patch("scripts.pg_tikv_admin_api.get_manager")
    def test_list_tenants_authorized(self, mock_get_manager, auth_headers):
        mock_manager = MagicMock()
        mock_manager.list_tenants.return_value = [
            {"name": "tenant1", "state": "ENABLED"},
            {"name": "tenant2", "state": "ENABLED"},
        ]
        mock_get_manager.return_value = mock_manager

        response = client.get("/api/tenants", headers=auth_headers)
        assert response.status_code == 200
        assert len(response.json()) == 2

    def test_create_tenant_validation(self, auth_headers):
        # Too short
        response = client.post(
            "/api/tenants",
            headers=auth_headers,
            json={"name": "ab"}
        )
        assert response.status_code == 422

        # Invalid characters
        response = client.post(
            "/api/tenants",
            headers=auth_headers,
            json={"name": "Invalid-Name"}
        )
        assert response.status_code == 422


class TestHealth:
    def test_health_no_auth_required(self):
        response = client.get("/api/health")
        assert response.status_code == 200
        assert "status" in response.json()
```

### 10.2 Frontend Tests

Using Vitest + React Testing Library:

```tsx
// admin-ui/src/__tests__/LoginPage.test.tsx
import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { BrowserRouter } from "react-router-dom"
import { LoginPage } from "@/pages/LoginPage"

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false } },
})

const wrapper = ({ children }) => (
  <QueryClientProvider client={queryClient}>
    <BrowserRouter>{children}</BrowserRouter>
  </QueryClientProvider>
)

describe("LoginPage", () => {
  it("renders login form", () => {
    render(<LoginPage />, { wrapper })
    
    expect(screen.getByText(/pg-tikv/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/password/i)).toBeInTheDocument()
    expect(screen.getByRole("button", { name: /login/i })).toBeInTheDocument()
  })

  it("shows error on invalid password", async () => {
    render(<LoginPage />, { wrapper })
    
    fireEvent.change(screen.getByLabelText(/password/i), {
      target: { value: "wrong" },
    })
    fireEvent.click(screen.getByRole("button", { name: /login/i }))
    
    await waitFor(() => {
      expect(screen.getByText(/invalid/i)).toBeInTheDocument()
    })
  })
})
```

```tsx
// admin-ui/src/__tests__/TenantTable.test.tsx
import { render, screen } from "@testing-library/react"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { BrowserRouter } from "react-router-dom"
import { TenantTable } from "@/components/tenants/TenantTable"

const mockTenants = [
  { name: "tenant1", state: "ENABLED" },
  { name: "tenant2", state: "DISABLED" },
]

// Mock the useTenants hook
jest.mock("@/api/tenants", () => ({
  useTenants: () => ({
    data: mockTenants,
    isLoading: false,
  }),
}))

describe("TenantTable", () => {
  it("renders tenant data", () => {
    render(
      <QueryClientProvider client={new QueryClient()}>
        <BrowserRouter>
          <TenantTable />
        </BrowserRouter>
      </QueryClientProvider>
    )
    
    expect(screen.getByText("tenant1")).toBeInTheDocument()
    expect(screen.getByText("tenant2")).toBeInTheDocument()
    expect(screen.getByText("ENABLED")).toBeInTheDocument()
    expect(screen.getByText("DISABLED")).toBeInTheDocument()
  })
})
```

## 11. Security Considerations

### 11.1 Issues Resolved

| Issue | Solution |
|-------|----------|
| Password in URL | Use JWT token + Tenant Session with body/header |
| No API authentication | JWT Bearer authentication |
| CORS too permissive | Whitelist specific origins |
| No session expiry | JWT expiry + Tenant Session expiry |

### 11.2 Production Deployment Recommendations

1. **Change default password**: Set `PGTIKV_ADMIN_PASSWORD` environment variable
2. **Configure JWT secret**: Set `PGTIKV_JWT_SECRET` environment variable (auto-generated if not set)
3. **Enable HTTPS**: Use Nginx reverse proxy with TLS
4. **Restrict CORS**: Set `PGTIKV_CORS_ORIGINS` to production domain only
5. **Use Redis**: Replace in-memory tenant session storage with Redis for horizontal scaling

### 11.3 Nginx Configuration Example

```nginx
server {
    listen 443 ssl;
    server_name admin.example.com;

    ssl_certificate /etc/ssl/certs/admin.crt;
    ssl_certificate_key /etc/ssl/private/admin.key;

    # Frontend static files
    location / {
        root /var/www/admin-ui/dist;
        try_files $uri $uri/ /index.html;
    }

    # API proxy
    location /api {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

### 11.4 Docker Compose Example

```yaml
version: "3.8"

services:
  admin-api:
    build:
      context: .
      dockerfile: Dockerfile.admin-api
    environment:
      - PGTIKV_ADMIN_PASSWORD=${ADMIN_PASSWORD}
      - PGTIKV_JWT_SECRET=${JWT_SECRET}
      - PGTIKV_PD_ENDPOINTS=pd:2379
      - PGTIKV_PG_HOST=pg-tikv
      - PGTIKV_PG_PORT=5433
      - PGTIKV_CORS_ORIGINS=["https://admin.example.com"]
    ports:
      - "8080:8080"

  admin-ui:
    build:
      context: ./admin-ui
      dockerfile: Dockerfile
    ports:
      - "80:80"
```

## 12. Future Improvements (Out of Scope)

The following features are identified but intentionally excluded from this design:

1. **Audit Logging**: Record all administrative operations
2. **Multi-user RBAC**: Support multiple admin users with different permissions
3. **Tenant Metrics**: Display tenant statistics (tables, rows, storage)
4. **Real-time Updates**: WebSocket for live status updates
5. **Backup/Restore**: UI for tenant data backup and restore
6. **Rate Limiting**: Protect API from abuse

## 13. References

- [FastAPI Documentation](https://fastapi.tiangolo.com/)
- [shadcn/ui Documentation](https://ui.shadcn.com/)
- [TanStack Query Documentation](https://tanstack.com/query/latest)
- [TanStack Table Documentation](https://tanstack.com/table/latest)
- [pg-tikv Multi-tenancy Guide](../multi-tenancy.md)
- [pg-tikv Authentication Guide](../authentication.md)
- [JWT Introduction](https://jwt.io/introduction)
