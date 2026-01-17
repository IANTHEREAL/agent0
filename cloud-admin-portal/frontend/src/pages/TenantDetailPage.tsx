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
    if (!confirm(`Delete user "${username}"?`)) return

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
      toast({
        title: "Password Reset",
        description: (
          <div className="mt-2">
            <p>New password: <code className="bg-muted px-1 rounded">{result.password}</code></p>
            <p className="text-xs mt-1 text-muted-foreground">Save this password - it won't be shown again.</p>
          </div>
        ),
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
    <div className="space-y-6">
      <div>
        <Button variant="ghost" asChild className="mb-4">
          <Link to="/tenants">
            <ArrowLeft className="w-4 h-4 mr-2" />
            Back to Tenants
          </Link>
        </Button>
        <div className="flex items-center gap-4">
          <h1 className="text-3xl font-bold">{name}</h1>
          <span
            className={cn(
              "inline-flex items-center gap-2 px-3 py-1 rounded-full text-sm",
              tenant.state === "ENABLED"
                ? "bg-green-500/10 text-green-600"
                : "bg-red-500/10 text-red-600"
            )}
          >
            <span
              className={cn(
                "w-2 h-2 rounded-full",
                tenant.state === "ENABLED" ? "bg-green-500" : "bg-red-500"
              )}
            />
            {tenant.state}
          </span>
        </div>
      </div>

      {/* Connection Info */}
      <Card>
        <CardHeader>
          <CardTitle>Connection Info</CardTitle>
          <CardDescription>Connect to this tenant using psql or any PostgreSQL client</CardDescription>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <p className="text-muted-foreground">Host</p>
              <p className="font-medium">{tenant.host || '127.0.0.1'}</p>
            </div>
            <div>
              <p className="text-muted-foreground">Port</p>
              <p className="font-medium">{tenant.port || 5433}</p>
            </div>
            <div className="col-span-2">
              <p className="text-muted-foreground mb-2">Connection Command</p>
              <div className="flex items-center gap-2">
                <code className="flex-1 bg-muted px-3 py-2 rounded text-sm">
                  psql -h {tenant.host || '127.0.0.1'} -p {tenant.port || 5433} -U {name}.admin
                </code>
                <Button variant="outline" size="sm" onClick={copyConnectionString}>
                  {copied ? <Check className="w-4 h-4" /> : <Copy className="w-4 h-4" />}
                </Button>
              </div>
            </div>
          </div>
        </CardContent>
      </Card>

      {/* User Management */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle>Users</CardTitle>
            <CardDescription>Manage database users for this tenant</CardDescription>
          </div>
          {isConnected && (
            <div className="flex gap-2">
              <Button size="sm" onClick={() => setShowCreateUser(true)}>
                <Plus className="w-4 h-4 mr-1" />
                Add User
              </Button>
              <Button variant="outline" size="sm" onClick={disconnect}>
                Disconnect
              </Button>
            </div>
          )}
        </CardHeader>
        <CardContent>
          {!isConnected ? (
            <form onSubmit={handleConnect} className="space-y-4">
              <p className="text-sm text-muted-foreground">
                Connect with admin credentials to manage users
              </p>
              <div className="grid grid-cols-2 gap-4">
                <div className="space-y-2">
                  <Label htmlFor="admin_user">Admin User</Label>
                  <Input
                    id="admin_user"
                    value={adminUser}
                    onChange={(e) => setAdminUser(e.target.value)}
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="admin_password">Admin Password</Label>
                  <Input
                    id="admin_password"
                    type="password"
                    value={adminPassword}
                    onChange={(e) => setAdminPassword(e.target.value)}
                    placeholder="Enter password"
                  />
                </div>
              </div>
              <Button type="submit" disabled={isConnecting}>
                {isConnecting ? "Connecting..." : "Connect"}
              </Button>
            </form>
          ) : usersLoading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin rounded-full h-6 w-6 border-b-2 border-primary"></div>
            </div>
          ) : (
            <div className="border rounded-lg">
              <table className="w-full">
                <thead>
                  <tr className="border-b bg-muted/50">
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">Name</th>
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">Superuser</th>
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">Login</th>
                    <th className="text-right p-4 text-sm font-medium text-muted-foreground">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {users?.map((user) => (
                    <tr key={user.name} className="border-b last:border-0 hover:bg-muted/50">
                      <td className="p-4 font-medium">{user.name}</td>
                      <td className="p-4">{user.is_superuser ? "Yes" : "No"}</td>
                      <td className="p-4">{user.can_login ? "Yes" : "No"}</td>
                      <td className="p-4">
                        <div className="flex justify-end gap-2">
                          <Button
                            variant="ghost"
                            size="sm"
                            onClick={() => handleResetPassword(user.name)}
                          >
                            <Key className="w-4 h-4 mr-1" />
                            Reset
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="text-destructive hover:text-destructive"
                            onClick={() => handleDeleteUser(user.name)}
                          >
                            <Trash2 className="w-4 h-4" />
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
    </div>
  )
}
