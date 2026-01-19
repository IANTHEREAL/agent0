import { useState } from "react"
import { Database, User, Key, Loader2 } from "lucide-react"
import { bootstrapTenantObservabilityUser, useCreateTenant } from "@/api/tenants"
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
        admin_user: adminUser,
        admin_password: adminPassword || undefined,
      })

      setCreatedTenant(result)
      setShowCredentials(true)
      onOpenChange(false)

      void bootstrapTenantObservabilityUser(
        result.id,
        result.admin_user,
        result.admin_password
      ).catch(() => {
        toast({
          title: "Warning",
          description:
            "Tenant created, but observability account bootstrap failed. Metrics may be unavailable until bootstrapped.",
          variant: "destructive",
        })
      })

      toast({
        title: "Tenant Created",
        description: `Tenant "${result.id}" has been created successfully.`,
      })

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
        <DialogContent className="sm:max-w-[440px]">
          <DialogHeader className="pb-4">
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 rounded-full bg-primary/10 flex items-center justify-center">
                <Database className="w-5 h-5 text-primary" />
              </div>
              <div>
                <DialogTitle>Create Tenant</DialogTitle>
                <DialogDescription className="text-xs mt-0.5">
                  Set up a new isolated database environment
                </DialogDescription>
              </div>
            </div>
          </DialogHeader>

          <form onSubmit={handleSubmit} className="space-y-5">
            <div className="space-y-4">
              <div className="space-y-3">
                <p className="text-xs font-medium text-muted-foreground flex items-center gap-2">
                  <User className="w-3.5 h-3.5" />
                  Admin Credentials
                </p>
                <div className="grid grid-cols-2 gap-3">
                  <div className="space-y-2">
                    <div className="h-4 flex items-center">
                      <Label htmlFor="admin_user" className="text-xs">Username</Label>
                    </div>
                    <Input
                      id="admin_user"
                      value={adminUser}
                      onChange={(e) => setAdminUser(e.target.value)}
                      className="h-9"
                      placeholder="admin"
                    />
                  </div>
                  <div className="space-y-2">
                    <div className="h-4 flex items-center justify-between">
                      <Label htmlFor="admin_password" className="text-xs">Password</Label>
                      {!adminPassword && (
                        <span className="text-[10px] text-muted-foreground font-normal flex items-center gap-1">
                          <Key className="w-2.5 h-2.5" />
                          Auto
                        </span>
                      )}
                    </div>
                    <Input
                      id="admin_password"
                      type="password"
                      value={adminPassword}
                      onChange={(e) => setAdminPassword(e.target.value)}
                      className="h-9"
                      placeholder="••••••••"
                    />
                  </div>
                </div>
                <p className="text-[11px] text-muted-foreground leading-relaxed">
                  A unique tenant ID will be auto-generated. Leave password empty to auto-generate.
                </p>
              </div>
            </div>

            <DialogFooter className="gap-2 sm:gap-2">
              <Button
                type="button"
                variant="ghost"
                onClick={() => onOpenChange(false)}
                className="text-muted-foreground"
              >
                Cancel
              </Button>
              <Button type="submit" disabled={mutation.isPending} className="gap-2 min-w-[120px]">
                {mutation.isPending ? (
                  <>
                    <Loader2 className="w-4 h-4 animate-spin" />
                    Creating...
                  </>
                ) : (
                  "Create Tenant"
                )}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {createdTenant && (
        <CredentialsModal
          open={showCredentials}
          onOpenChange={setShowCredentials}
          title="Tenant Created Successfully"
          description={`Tenant "${createdTenant.id}" has been created. Save these credentials - they won't be shown again.`}
          credentials={[
            {
              label: "Tenant ID",
              value: createdTenant.id,
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
