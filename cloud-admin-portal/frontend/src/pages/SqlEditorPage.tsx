import { useParams } from "react-router-dom"
import { Lock, LogIn, Loader2 } from "lucide-react"
import { useTenantSessionContext } from "@/contexts/TenantSessionContext"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useToast } from "@/components/ui/use-toast"
import { SqlEditor } from "@/components/sql/SqlEditor"

export function SqlEditorPage() {
  const { id: tenantId } = useParams<{ id: string }>()
  const {
    isConnected,
    adminUser,
    adminPassword,
    connect,
    disconnect,
    isConnecting,
    setAdminUser,
    setAdminPassword,
  } = useTenantSessionContext()
  const { toast } = useToast()

  if (!tenantId) {
    return (
      <div className="text-center py-12">
        <p className="text-lg font-medium">Tenant not found</p>
      </div>
    )
  }

  const handleConnect = async (e: React.FormEvent) => {
    e.preventDefault()
    try {
      await connect(adminUser, adminPassword)
      toast({ title: "Connected", description: "Successfully connected to tenant" })
    } catch {
      toast({ title: "Connection Failed", description: "Invalid credentials or tenant not accessible", variant: "destructive" })
    }
  }

  if (!isConnected) {
    return (
      <div className="flex items-center justify-center py-16">
        <div className="w-full max-w-sm rounded-lg border border-dashed border-border/60 bg-muted/10 p-6">
          <div className="flex flex-col items-center text-center mb-6">
            <div className="w-12 h-12 rounded-full bg-primary/10 flex items-center justify-center mb-3">
              <Lock className="w-6 h-6 text-primary/60" />
            </div>
            <h3 className="text-sm font-medium mb-1">Authentication Required</h3>
            <p className="text-xs text-muted-foreground max-w-[280px]">
              Connect with admin credentials to use the SQL editor
            </p>
          </div>
          <form onSubmit={handleConnect} className="space-y-4">
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <Label htmlFor="sql_admin_user" className="text-xs font-medium">Username</Label>
                <Input
                  id="sql_admin_user"
                  className="h-9 text-sm"
                  value={adminUser}
                  onChange={(e) => setAdminUser(e.target.value)}
                  placeholder="admin"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="sql_admin_password" className="text-xs font-medium">Password</Label>
                <Input
                  id="sql_admin_password"
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
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className="flex items-center gap-1 text-[10px] font-medium text-green-600 bg-green-500/10 px-2 py-0.5 rounded-full">
            <span className="w-1.5 h-1.5 rounded-full bg-green-500 animate-pulse" />
            Connected as {adminUser}
          </span>
        </div>
        <Button variant="ghost" size="sm" className="h-7 text-xs text-muted-foreground hover:text-foreground" onClick={disconnect}>
          Disconnect
        </Button>
      </div>
      <SqlEditor tenantId={tenantId} />
    </div>
  )
}
