/**
 * Tenant detail page with user management
 */

import { useState } from "react"
import { useParams, Link } from "react-router-dom"
import { ArrowLeft, Key, Trash2, Copy, Check, Plus, Network, Users, Shield, LogIn, Lock, Loader2 } from "lucide-react"
import { useTenant } from "@/api/tenants"
import { useUsers, useDeleteUser, useResetPassword } from "@/api/users"
import { useTenantSession } from "@/hooks/useTenantSession"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import { CreateUserDialog } from "@/components/users/CreateUserDialog"
import { CredentialsModal } from "@/components/common/CredentialsModal"
import { ConfirmDialog } from "@/components/common/ConfirmDialog"
import { SqlEditor } from "@/components/sql/SqlEditor"
import { TenantObservabilityCard } from "@/components/observability/TenantObservabilityCard"

export function TenantDetailPage() {
  const { id: tenantId } = useParams<{ id: string }>()
  const { data: tenant, isLoading: tenantLoading } = useTenant(tenantId!)
  const { toast } = useToast()

  // Connection state
  const [adminUser, setAdminUser] = useState("admin")
  const [adminPassword, setAdminPassword] = useState("")
  const {
    isConnected,
    connect,
    disconnect,
    isConnecting,
  } = useTenantSession(tenantId!)

  // User management
  const { data: users, isLoading: usersLoading } = useUsers(tenantId!, isConnected)
  const deleteUserMutation = useDeleteUser(tenantId!)
  const resetPasswordMutation = useResetPassword(tenantId!)

  const [copied, setCopied] = useState(false)
  const [showCreateUser, setShowCreateUser] = useState(false)
  const [showPasswordModal, setShowPasswordModal] = useState(false)
  const [resetPasswordResult, setResetPasswordResult] = useState<{
    username: string
    password: string
  } | null>(null)
  const [confirmDeleteUser, setConfirmDeleteUser] = useState<string | null>(null)

  const handleConnect = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      await connect(adminUser, adminPassword)
      toast({
        title: "Connected",
        description: "Successfully connected to tenant",
      })
    } catch (error) {
      toast({
        title: "Connection Failed",
        description: "Invalid credentials or tenant not accessible",
        variant: "destructive",
      })
    }
  }

  const handleDeleteUser = async (username: string) => {
    try {
      await deleteUserMutation.mutateAsync(username)
      toast({
        title: "User Deleted",
        description: `User "${username}" has been deleted`,
      })
    } catch (error) {
      toast({
        title: "Error",
        description: "Failed to delete user",
        variant: "destructive",
      })
    }
  }

  const handleResetPassword = async (username: string) => {
    try {
      const result = await resetPasswordMutation.mutateAsync(username)

      // Save result and show credentials modal
      setResetPasswordResult({
        username,
        password: result.password,
      })
      setShowPasswordModal(true)

      // Show simple success toast
      toast({
        title: "Password Reset",
        description: `Password for user "${username}" has been reset.`,
      })
    } catch (error) {
      toast({
        title: "Error",
        description: "Failed to reset password",
        variant: "destructive",
      })
    }
  }

  const copyConnectionString = async (endpoint?: { host: string; port: number }) => {
    const host = endpoint?.host || tenant?.endpoints?.[0]?.host || '127.0.0.1'
    const port = endpoint?.port || tenant?.endpoints?.[0]?.port || 5433
    const connStr = `psql -h ${host} -p ${port} -U t${tenantId}.admin`
    
    try {
      await navigator.clipboard.writeText(connStr)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // Fallback for non-secure contexts or when clipboard API fails
      const textArea = document.createElement('textarea')
      textArea.value = connStr
      textArea.style.position = 'fixed'
      textArea.style.left = '-9999px'
      document.body.appendChild(textArea)
      textArea.select()
      try {
        document.execCommand('copy')
        setCopied(true)
        setTimeout(() => setCopied(false), 2000)
      } catch {
        toast({
          title: "Copy failed",
          description: "Please manually copy the connection string",
          variant: "destructive",
        })
      } finally {
        document.body.removeChild(textArea)
      }
    }
  }

  if (tenantLoading) {
    return (
      <div className="flex items-center justify-center py-12">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
      </div>
    )
  }

  if (!tenant) {
    return (
      <div className="text-center py-12">
        <p className="text-lg font-medium">Tenant not found</p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/tenants">Back to tenants</Link>
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <div>
        <Button variant="ghost" size="sm" asChild className="mb-3 h-8 text-xs">
          <Link to="/tenants">
            <ArrowLeft className="w-3.5 h-3.5 mr-1.5" />
            Back to Tenants
          </Link>
        </Button>
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-semibold font-mono">{tenantId}</h1>
          <span
            className={cn(
              "inline-flex items-center gap-1.5 px-2 py-0.5 rounded-full text-xs",
              tenant.state === "ENABLED"
                ? "bg-green-500/10 text-green-600"
                : "bg-red-500/10 text-red-600"
            )}
          >
            <span
              className={cn(
                "w-1.5 h-1.5 rounded-full",
                tenant.state === "ENABLED" ? "bg-green-500" : "bg-red-500"
              )}
            />
            {tenant.state}
          </span>
        </div>
      </div>

      {/* Connection Endpoints */}
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-center gap-2">
            <Network className="h-4 w-4 text-muted-foreground" />
            <CardTitle className="text-base">Connection Endpoints</CardTitle>
          </div>
          <CardDescription className="text-xs">
            {tenant.endpoints && tenant.endpoints.length > 1
              ? "Multiple endpoints available for load balancing"
              : "Connect using psql or any PostgreSQL-compatible client"}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {tenant.endpoints && tenant.endpoints.length > 0 ? (
            <div className="space-y-3">
              {tenant.endpoints.map((endpoint, idx) => (
                <div key={idx} className="group rounded-lg border border-border/50 bg-muted/20 hover:bg-muted/40 transition-colors overflow-hidden">
                  <div className="flex items-center justify-between px-4 py-2.5 border-b border-border/30 bg-muted/30">
                    <div className="flex items-center gap-2.5">
                      <span className="text-sm font-medium">
                        {endpoint.description || `Endpoint ${idx + 1}`}
                      </span>
                      <span className={cn(
                        "inline-flex items-center px-2 py-0.5 rounded-full text-[10px] font-medium uppercase tracking-wide",
                        endpoint.type === "primary" && "bg-blue-500/10 text-blue-600 ring-1 ring-blue-500/20",
                        endpoint.type === "replica" && "bg-emerald-500/10 text-emerald-600 ring-1 ring-emerald-500/20",
                        endpoint.type === "load_balancer" && "bg-violet-500/10 text-violet-600 ring-1 ring-violet-500/20"
                      )}>
                        {endpoint.type.replace('_', ' ')}
                      </span>
                      {endpoint.region && (
                        <span className="text-[10px] text-muted-foreground bg-muted px-1.5 py-0.5 rounded">
                          {endpoint.region}
                        </span>
                      )}
                    </div>
                    {!endpoint.enabled && (
                      <span className="text-[10px] font-medium text-red-500 bg-red-500/10 px-2 py-0.5 rounded-full">Offline</span>
                    )}
                  </div>
                  <div className="px-4 py-3 space-y-2.5">
                    <div className="flex items-center gap-6 text-xs">
                      <div className="flex items-center gap-1.5">
                        <span className="text-muted-foreground">Host</span>
                        <span className="font-mono font-medium bg-background/50 px-1.5 py-0.5 rounded">{endpoint.host}</span>
                      </div>
                      <div className="flex items-center gap-1.5">
                        <span className="text-muted-foreground">Port</span>
                        <span className="font-mono font-medium bg-background/50 px-1.5 py-0.5 rounded">{endpoint.port}</span>
                      </div>
                    </div>
                    <div className="flex items-center gap-2">
                      <code className="flex-1 bg-background/60 border border-border/30 px-3 py-2 rounded-md text-xs font-mono text-muted-foreground">
                        psql -h {endpoint.host} -p {endpoint.port} -U t{tenantId}.admin
                      </code>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-8 w-8 p-0 hover:bg-primary/10"
                        onClick={() => copyConnectionString(endpoint)}
                      >
                        {copied ? (
                          <Check className="w-4 h-4 text-green-500" />
                        ) : (
                          <Copy className="w-4 h-4 text-muted-foreground" />
                        )}
                      </Button>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className="flex flex-col items-center justify-center py-8 text-muted-foreground border border-dashed rounded-lg bg-muted/10">
              <Network className="w-8 h-8 mb-2 opacity-40" />
              <p className="text-sm">No connection endpoints configured</p>
            </div>
          )}
        </CardContent>
      </Card>

      {/* User Management */}
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Users className="h-4 w-4 text-muted-foreground" />
              <CardTitle className="text-base">Users</CardTitle>
              {isConnected && (
                <span className="flex items-center gap-1 text-[10px] font-medium text-green-600 bg-green-500/10 px-2 py-0.5 rounded-full">
                  <span className="w-1.5 h-1.5 rounded-full bg-green-500 animate-pulse" />
                  Connected
                </span>
              )}
            </div>
            {isConnected && (
              <div className="flex gap-2">
                <Button size="sm" className="h-7 text-xs gap-1.5" onClick={() => setShowCreateUser(true)}>
                  <Plus className="w-3.5 h-3.5" />
                  Add User
                </Button>
                <Button variant="ghost" size="sm" className="h-7 text-xs text-muted-foreground hover:text-foreground" onClick={disconnect}>
                  Disconnect
                </Button>
              </div>
            )}
          </div>
          <CardDescription className="text-xs">
            {isConnected ? "Manage database users and their permissions" : "Connect with admin credentials to manage users"}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {!isConnected ? (
            <div className="rounded-lg border border-dashed border-border/60 bg-muted/10 p-6">
              <div className="flex flex-col items-center text-center mb-6">
                <div className="w-12 h-12 rounded-full bg-primary/10 flex items-center justify-center mb-3">
                  <Lock className="w-6 h-6 text-primary/60" />
                </div>
                <h3 className="text-sm font-medium mb-1">Authentication Required</h3>
                <p className="text-xs text-muted-foreground max-w-[280px]">
                  Enter your admin credentials to access user management features
                </p>
              </div>
              <form onSubmit={handleConnect} className="space-y-4 max-w-sm mx-auto">
                <div className="grid grid-cols-2 gap-3">
                  <div className="space-y-1.5">
                    <Label htmlFor="admin_user" className="text-xs font-medium">Username</Label>
                    <Input
                      id="admin_user"
                      className="h-9 text-sm"
                      value={adminUser}
                      onChange={(e) => setAdminUser(e.target.value)}
                      placeholder="admin"
                    />
                  </div>
                  <div className="space-y-1.5">
                    <Label htmlFor="admin_password" className="text-xs font-medium">Password</Label>
                    <Input
                      id="admin_password"
                      type="password"
                      className="h-9 text-sm"
                      value={adminPassword}
                      onChange={(e) => setAdminPassword(e.target.value)}
                      placeholder="••••••••"
                    />
                  </div>
                </div>
                <Button type="submit" size="sm" className="w-full h-9 gap-2" disabled={isConnecting}>
                  {isConnecting ? (
                    <>
                      <Loader2 className="w-4 h-4 animate-spin" />
                      Connecting...
                    </>
                  ) : (
                    <>
                      <LogIn className="w-4 h-4" />
                      Connect
                    </>
                  )}
                </Button>
              </form>
            </div>
          ) : usersLoading ? (
            <div className="flex flex-col items-center justify-center py-12">
              <Loader2 className="w-6 h-6 animate-spin text-primary mb-2" />
              <p className="text-xs text-muted-foreground">Loading users...</p>
            </div>
          ) : users && users.length > 0 ? (
            <div className="rounded-lg border border-border/50 overflow-hidden">
              <table className="w-full">
                <thead>
                  <tr className="border-b bg-muted/30">
                    <th className="text-left px-4 py-2.5 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">User</th>
                    <th className="text-center px-4 py-2.5 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">Superuser</th>
                    <th className="text-center px-4 py-2.5 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">Can Login</th>
                    <th className="text-right px-4 py-2.5 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {users.map((user, idx) => (
                    <tr key={user.name} className={cn(
                      "hover:bg-muted/40 transition-colors",
                      idx !== users.length - 1 && "border-b border-border/30"
                    )}>
                      <td className="px-4 py-3">
                        <div className="flex items-center gap-2">
                          <div className="w-7 h-7 rounded-full bg-primary/10 flex items-center justify-center">
                            <span className="text-xs font-medium text-primary">{user.name.charAt(0).toUpperCase()}</span>
                          </div>
                          <span className="text-sm font-medium">{user.name}</span>
                        </div>
                      </td>
                      <td className="px-4 py-3 text-center">
                        {user.is_superuser ? (
                          <span className="inline-flex items-center gap-1 text-[10px] font-medium text-amber-600 bg-amber-500/10 px-2 py-0.5 rounded-full">
                            <Shield className="w-3 h-3" />
                            Yes
                          </span>
                        ) : (
                          <span className="text-xs text-muted-foreground">No</span>
                        )}
                      </td>
                      <td className="px-4 py-3 text-center">
                        {user.can_login ? (
                          <span className="inline-flex items-center gap-1 text-[10px] font-medium text-green-600 bg-green-500/10 px-2 py-0.5 rounded-full">
                            <Check className="w-3 h-3" />
                            Yes
                          </span>
                        ) : (
                          <span className="text-xs text-muted-foreground">No</span>
                        )}
                      </td>
                      <td className="px-4 py-3">
                        <div className="flex justify-end gap-1">
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 text-xs gap-1 text-muted-foreground hover:text-foreground"
                            onClick={() => handleResetPassword(user.name)}
                          >
                            <Key className="w-3.5 h-3.5" />
                            Reset
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 w-7 p-0 text-muted-foreground hover:text-destructive hover:bg-destructive/10"
                            onClick={() => setConfirmDeleteUser(user.name)}
                          >
                            <Trash2 className="w-3.5 h-3.5" />
                          </Button>
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <div className="flex flex-col items-center justify-center py-8 text-muted-foreground border border-dashed rounded-lg bg-muted/10">
              <Users className="w-8 h-8 mb-2 opacity-40" />
              <p className="text-sm">No users found</p>
              <Button size="sm" variant="link" className="mt-1 h-auto p-0 text-xs" onClick={() => setShowCreateUser(true)}>
                Create the first user
              </Button>
            </div>
          )}
        </CardContent>
      </Card>

      <TenantObservabilityCard tenantId={tenantId!} />

      {isConnected && <SqlEditor tenantId={tenantId!} />}

      <CreateUserDialog
        tenantId={tenantId!}
        open={showCreateUser}
        onOpenChange={setShowCreateUser}
      />

      {/* Password Reset Modal */}
      {resetPasswordResult && (
        <CredentialsModal
          open={showPasswordModal}
          onOpenChange={setShowPasswordModal}
          title="Password Reset Successfully"
          description={`New password for user "${resetPasswordResult.username}"`}
          credentials={[
            {
              label: "Username",
              value: resetPasswordResult.username,
              copyable: true,
            },
            {
              label: "New Password",
              value: resetPasswordResult.password,
              sensitive: true,
              copyable: true,
            },
          ]}
        />
      )}

      {/* Delete User Confirmation */}
      <ConfirmDialog
        open={confirmDeleteUser !== null}
        onOpenChange={(open) => !open && setConfirmDeleteUser(null)}
        title="Delete User?"
        description={`This will permanently delete user "${confirmDeleteUser}" and revoke all access. This action cannot be undone.`}
        confirmLabel="Delete User"
        variant="destructive"
        onConfirm={() => handleDeleteUser(confirmDeleteUser!)}
      />
    </div>
  )
}
