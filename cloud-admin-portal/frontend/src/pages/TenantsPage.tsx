/**
 * Tenants list page
 */

import { useState } from "react"
import { Link } from "react-router-dom"
import { Plus, Ban, Users, Edit } from "lucide-react"
import { useTenants, useRemoveTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import { CreateTenantDialog } from "@/components/tenants/CreateTenantDialog"
import { EditTenantMetadataDialog } from "@/components/tenants/EditTenantMetadataDialog"
import { ConfirmDialog } from "@/components/common/ConfirmDialog"
import type { Tenant } from "@/types"

export function TenantsPage() {
  const [showCreateDialog, setShowCreateDialog] = useState(false)
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null)
  const [editingTenant, setEditingTenant] = useState<Tenant | null>(null)
  const { data: tenants, isLoading } = useTenants()
  const removeMutation = useRemoveTenant()
  const { toast } = useToast()

  const handleRemove = async (name: string) => {
    try {
      await removeMutation.mutateAsync(name)
      toast({
        title: "Tenant Removed",
        description: `Tenant "${name}" has been removed from the portal.`,
      })
    } catch (error) {
      toast({
        title: "Error",
        description: "Failed to remove tenant",
        variant: "destructive",
      })
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex justify-between items-center">
        <div>
          <h1 className="text-3xl font-bold">Tenants</h1>
          <p className="text-muted-foreground">
            Manage your multi-tenant database instances
          </p>
        </div>
        <Button onClick={() => setShowCreateDialog(true)}>
          <Plus className="w-4 h-4 mr-2" />
          New Tenant
        </Button>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>All Tenants</CardTitle>
          <CardDescription>View and manage tenant keyspaces</CardDescription>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="flex items-center justify-center py-12">
              <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
            </div>
          ) : !tenants?.length ? (
            <div className="text-center py-12 text-muted-foreground">
              <p className="text-lg font-medium">No tenants yet</p>
              <p className="text-sm">Create your first tenant to get started</p>
            </div>
          ) : (
            <div className="border rounded-lg">
              <table className="w-full">
                <thead>
                  <tr className="border-b bg-muted/50">
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">
                      Name
                    </th>
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">
                      Status
                    </th>
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">
                      Tags
                    </th>
                    <th className="text-left p-4 text-sm font-medium text-muted-foreground">
                      Connection
                    </th>
                    <th className="text-right p-4 text-sm font-medium text-muted-foreground">
                      Actions
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {tenants.map((tenant) => (
                    <tr key={tenant.name} className="border-b last:border-0 hover:bg-muted/50">
                      <td className="p-4">
                        <Link
                          to={`/tenants/${tenant.name}`}
                          className="font-medium hover:underline"
                        >
                          {tenant.name}
                        </Link>
                      </td>
                      <td className="p-4">
                        <span
                          className={cn(
                            "inline-flex items-center gap-2 text-sm",
                            tenant.state === "ENABLED"
                              ? "text-green-600"
                              : "text-red-600"
                          )}
                        >
                          <span
                            className={cn(
                              "w-2 h-2 rounded-full",
                              tenant.state === "ENABLED"
                                ? "bg-green-500"
                                : "bg-red-500"
                            )}
                          />
                          {tenant.state}
                        </span>
                      </td>
                      <td className="p-4">
                        {tenant.tags && tenant.tags.length > 0 ? (
                          <div className="flex flex-wrap gap-1">
                            {tenant.tags.map((tag) => (
                              <span
                                key={tag}
                                className="inline-flex items-center px-2 py-0.5 rounded-full text-xs bg-blue-100 text-blue-700"
                              >
                                {tag}
                              </span>
                            ))}
                          </div>
                        ) : (
                          <span className="text-xs text-muted-foreground">-</span>
                        )}
                      </td>
                      <td className="p-4">
                        <code className="text-sm bg-muted px-2 py-1 rounded">
                          {tenant.name}.&lt;user&gt;
                        </code>
                      </td>
                      <td className="p-4">
                        <div className="flex justify-end gap-2">
                          <Button
                            variant="ghost"
                            size="sm"
                            asChild
                          >
                            <Link to={`/tenants/${tenant.name}`}>
                              <Users className="w-4 h-4 mr-1" />
                              Users
                            </Link>
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            onClick={() => setEditingTenant(tenant)}
                          >
                            <Edit className="w-4 h-4" />
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="text-destructive hover:text-destructive"
                            onClick={() => setConfirmRemove(tenant.name)}
                          >
                            <Ban className="w-4 h-4" />
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

      <CreateTenantDialog
        open={showCreateDialog}
        onOpenChange={setShowCreateDialog}
      />

      <EditTenantMetadataDialog
        tenant={editingTenant}
        open={editingTenant !== null}
        onOpenChange={(open) => !open && setEditingTenant(null)}
      />

      <ConfirmDialog
        open={confirmRemove !== null}
        onOpenChange={(open) => !open && setConfirmRemove(null)}
        title="Remove Tenant?"
        description={
          <div className="space-y-2">
            <p>
              This will remove tenant <strong>{confirmRemove}</strong> from the portal interface.
            </p>
            <p className="text-sm text-muted-foreground">
              The tenant's keyspace will be disabled in TiKV and hidden from this portal.
              Data remains in storage but becomes inaccessible. This action cannot be undone.
            </p>
          </div>
        }
        confirmLabel="Remove Tenant"
        variant="destructive"
        onConfirm={() => handleRemove(confirmRemove!)}
      />
    </div>
  )
}
