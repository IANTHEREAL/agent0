/**
 * Create tenant dialog
 */

import { useState } from "react"
import { useCreateTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { useToast } from "@/components/ui/use-toast"
import { ApiError } from "@/api/client"
import { CredentialsModal } from "@/components/common/CredentialsModal"
import type { TenantCreateResponse } from "@/types"

interface CreateTenantDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CreateTenantDialog({ open, onOpenChange }: CreateTenantDialogProps) {
  const [name, setName] = useState("")
  const [adminUser, setAdminUser] = useState("admin")
  const [adminPassword, setAdminPassword] = useState("")
  const [showCredentials, setShowCredentials] = useState(false)
  const [createdTenant, setCreatedTenant] = useState<TenantCreateResponse | null>(null)

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

      // Save result and show credentials modal
      setCreatedTenant(result)
      setShowCredentials(true)

      // Close creation dialog
      onOpenChange(false)

      // Show simple success toast
      toast({
        title: "Tenant Created",
        description: `Tenant "${result.name}" has been created successfully.`,
      })

      // Reset form
      setName("")
      setAdminUser("admin")
      setAdminPassword("")
    } catch (error) {
      const message = error instanceof ApiError ? error.message : "Failed to create tenant"
      toast({
        title: "Error",
        description: message,
        variant: "destructive",
      })
    }
  }

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Create Tenant</DialogTitle>
            <DialogDescription>
              Create a new isolated database tenant with its own keyspace.
            </DialogDescription>
          </DialogHeader>

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
                  type="password"
                  value={adminPassword}
                  onChange={(e) => setAdminPassword(e.target.value)}
                  placeholder="Auto-generate"
                />
              </div>
            </div>

            <DialogFooter>
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
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Credentials Display Modal */}
      {createdTenant && (
        <CredentialsModal
          open={showCredentials}
          onOpenChange={setShowCredentials}
          title="Tenant Created Successfully"
          description={`Tenant "${createdTenant.name}" has been created. Save these credentials - they won't be shown again.`}
          credentials={[
            {
              label: "Tenant Name",
              value: createdTenant.name,
              copyable: true,
            },
            {
              label: "Admin Username",
              value: createdTenant.admin_user,
              copyable: true,
            },
            {
              label: "Admin Password",
              value: createdTenant.admin_password,
              sensitive: true,
              copyable: true,
            },
          ]}
          connectionCommand={createdTenant.connection_string}
        />
      )}
    </>
  )
}
