/**
 * Create tenant dialog
 */

import { useState } from "react"
import { useCreateTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useToast } from "@/components/ui/use-toast"
import { ApiError } from "@/api/client"

interface CreateTenantDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CreateTenantDialog({ open, onOpenChange }: CreateTenantDialogProps) {
  const [name, setName] = useState("")
  const [adminUser, setAdminUser] = useState("admin")
  const [adminPassword, setAdminPassword] = useState("")
  const mutation = useCreateTenant()
  const { toast } = useToast()

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()

    try {
      const result = await mutation.mutateAsync({
        name,
        admin_user: adminUser,
        admin_password: adminPassword || undefined,
      })

      toast({
        title: "Tenant Created",
        description: (
          <div className="mt-2 space-y-2">
            <p>Tenant: <strong>{result.name}</strong></p>
            <p>Password: <code className="bg-muted px-1 rounded">{result.admin_password}</code></p>
            <p className="text-xs text-muted-foreground mt-2">
              Save this password - it won't be shown again.
            </p>
          </div>
        ),
      })

      // Reset form
      setName("")
      setAdminUser("admin")
      setAdminPassword("")
      onOpenChange(false)
    } catch (error) {
      const message = error instanceof ApiError ? error.message : "Failed to create tenant"
      toast({
        title: "Error",
        description: message,
        variant: "destructive",
      })
    }
  }

  if (!open) return null

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        className="fixed inset-0 bg-black/50"
        onClick={() => onOpenChange(false)}
      />
      <div className="relative bg-card border rounded-lg shadow-lg w-full max-w-md p-6 z-50">
        <h2 className="text-lg font-semibold mb-1">Create Tenant</h2>
        <p className="text-sm text-muted-foreground mb-4">
          Create a new isolated database tenant with its own keyspace.
        </p>

        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="tenant_name">Tenant Name</Label>
            <Input
              id="tenant_name"
              value={name}
              onChange={(e) => setName(e.target.value.toLowerCase())}
              placeholder="acme_corp"
              pattern="[a-z0-9_]+"
              minLength={3}
              maxLength={64}
              required
            />
            <p className="text-xs text-muted-foreground">
              Lowercase letters, numbers, and underscores (3-64 chars)
            </p>
          </div>

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
              <Label htmlFor="admin_password">Password</Label>
              <Input
                id="admin_password"
                value={adminPassword}
                onChange={(e) => setAdminPassword(e.target.value)}
                placeholder="Auto-generate"
              />
            </div>
          </div>

          <div className="flex justify-end gap-2 pt-4">
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
      </div>
    </div>
  )
}
