/**
 * Tenant detail page with user management
 */

import { useState } from "react"
import { useParams, Link } from "react-router-dom"
import { ArrowLeft, Key, Trash2, Copy, Check, Plus } from "lucide-react"
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

export function TenantDetailPage() {
  const { name } = useParams<{ name: string }>()
  const { data: tenant, isLoading: tenantLoading } = useTenant(name!)
  const { toast } = useToast()

  // Connection state
  const [adminUser, setAdminUser] = useState("admin")
  const [adminPassword, setAdminPassword] = useState("")
  const {
    isConnected,
    connect,
    disconnect,
    isConnecting,
  } = useTenantSession(name!)

  // User management
  const { data: users, isLoading: usersLoading } = useUsers(name!, isConnected)
  const deleteUserMutation = useDeleteUser(name!)
  const resetPasswordMutation = useResetPassword(name!)

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

  const copyConnectionString = () => {
    const connStr = `psql -h ${tenant?.host || '127.0.0.1'} -p ${tenant?.port || 5433} -U ${name}.admin`
    navigator.clipboard.writeText(connStr)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
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
          <h1 className="text-2xl font-semibold">{name}</h1>
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

      {/* Connection Info */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">Connection Info</CardTitle>
          <CardDescription className="text-xs">Connect to this tenant using psql or any PostgreSQL client</CardDescription>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 gap-3 text-sm">
            <div>
              <p className="text-xs text-muted-foreground mb-1">Host</p>
              <p className="text-sm font-medium">{tenant.host || '127.0.0.1'}</p>
            </div>
            <div>
              <p className="text-xs text-muted-foreground mb-1">Port</p>
              <p className="text-sm font-medium">{tenant.port || 5433}</p>
            </div>
            <div className="col-span-2">
              <p className="text-xs text-muted-foreground mb-1.5">Connection Command</p>
              <div className="flex items-center gap-2">
                <code className="flex-1 bg-muted px-2.5 py-1.5 rounded text-xs">
                  psql -h {tenant.host || '127.0.0.1'} -p {tenant.port || 5433} -U {name}.admin
                </code>
                <Button variant="outline" size="sm" className="h-7 w-7 p-0" onClick={copyConnectionString}>
                  {copied ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
                </Button>
              </div>
            </div>
          </div>
        </CardContent>
      </Card>

      {/* User Management */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between pb-3">
          <div>
            <CardTitle className="text-base">Users</CardTitle>
            <CardDescription className="text-xs">Manage database users for this tenant</CardDescription>
          </div>
          {isConnected && (
            <div className="flex gap-2">
              <Button size="sm" className="h-7 text-xs" onClick={() => setShowCreateUser(true)}>
                <Plus className="w-3.5 h-3.5 mr-1" />
                Add User
              </Button>
              <Button variant="outline" size="sm" className="h-7 text-xs" onClick={disconnect}>
                Disconnect
              </Button>
            </div>
          )}
        </CardHeader>
        <CardContent>
          {!isConnected ? (
            <form onSubmit={handleConnect} className="space-y-3">
              <p className="text-xs text-muted-foreground">
                Connect with admin credentials to manage users
              </p>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-1.5">
                  <Label htmlFor="admin_user" className="text-xs">Admin User</Label>
                  <Input
                    id="admin_user"
                    className="h-8 text-sm"
                    value={adminUser}
                    onChange={(e) => setAdminUser(e.target.value)}
                  />
                </div>
                <div className="space-y-1.5">
                  <Label htmlFor="admin_password" className="text-xs">Admin Password</Label>
                  <Input
                    id="admin_password"
                    type="password"
                    className="h-8 text-sm"
                    value={adminPassword}
                    onChange={(e) => setAdminPassword(e.target.value)}
                    placeholder="Enter password"
                  />
                </div>
              </div>
              <Button type="submit" size="sm" className="h-8 text-xs" disabled={isConnecting}>
                {isConnecting ? "Connecting..." : "Connect"}
              </Button>
            </form>
          ) : usersLoading ? (
            <div className="flex items-center justify-center py-6">
              <div className="animate-spin rounded-full h-5 w-5 border-b-2 border-primary"></div>
            </div>
          ) : (
            <div className="border rounded-lg">
              <table className="w-full">
                <thead>
                  <tr className="border-b bg-muted/50">
                    <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Name</th>
                    <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Superuser</th>
                    <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Login</th>
                    <th className="text-right px-3 py-2 text-xs font-medium text-muted-foreground">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {users?.map((user) => (
                    <tr key={user.name} className="border-b last:border-0 hover:bg-muted/50">
                      <td className="px-3 py-2.5 text-sm font-medium">{user.name}</td>
                      <td className="px-3 py-2.5 text-xs">{user.is_superuser ? "Yes" : "No"}</td>
                      <td className="px-3 py-2.5 text-xs">{user.can_login ? "Yes" : "No"}</td>
                      <td className="px-3 py-2.5">
                        <div className="flex justify-end gap-1">
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 text-xs"
                            onClick={() => handleResetPassword(user.name)}
                          >
                            <Key className="w-3.5 h-3.5 mr-1" />
                            Reset
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 w-7 p-0 text-destructive hover:text-destructive"
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
          )}
        </CardContent>
      </Card>

      <CreateUserDialog
        tenantName={name!}
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
