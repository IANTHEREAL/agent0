/**
 * Tenants list page
 */

import { useState } from "react"
import { Link } from "react-router-dom"
import { Plus, Ban, Users, Edit } from "lucide-react"
import { useTenants, useRemoveTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { SortableHeader } from "@/components/ui/sortable-header"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import { useSortableData } from "@/hooks/useSortableData"
import { CreateTenantDialog } from "@/components/tenants/CreateTenantDialog"
import { EditTenantMetadataDialog } from "@/components/tenants/EditTenantMetadataDialog"
import { ConfirmDialog } from "@/components/common/ConfirmDialog"
import type { Tenant } from "@/types"

type TenantSortKey = "id" | "state" | "created_at" | "tags"

const tenantSortColumns = [
  { key: "id" as const, getValue: (t: Tenant) => parseInt(t.id, 10) },
  { key: "state" as const, getValue: (t: Tenant) => t.state },
  { key: "created_at" as const, getValue: (t: Tenant) => t.created_at ? new Date(t.created_at) : null },
  { key: "tags" as const, getValue: (t: Tenant) => t.tags ?? [] },
]

export function TenantsPage() {
  const [showCreateDialog, setShowCreateDialog] = useState(false)
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null)
  const [editingTenant, setEditingTenant] = useState<Tenant | null>(null)
  const { data: tenants, isLoading } = useTenants()
  const removeMutation = useRemoveTenant()
  const { toast } = useToast()

  const { sortedData: sortedTenants, requestSort, getSortDirection } = useSortableData<Tenant, TenantSortKey>(
    tenants,
    tenantSortColumns
  )

  const handleRemove = async (tenantId: string) => {
    try {
      await removeMutation.mutateAsync(tenantId)
      toast({
        title: "Tenant Removed",
        description: `Tenant "${tenantId}" has been removed from the portal.`,
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
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <div>
          <h1 className="text-2xl font-semibold">Tenants</h1>
          <p className="text-sm text-muted-foreground mt-0.5">
            Manage your multi-tenant database instances
          </p>
        </div>
        <Button size="sm" onClick={() => setShowCreateDialog(true)}>
          <Plus className="w-4 h-4 mr-2" />
          New Tenant
        </Button>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">All Tenants</CardTitle>
          <CardDescription className="text-xs">View and manage tenant keyspaces</CardDescription>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin rounded-full h-6 w-6 border-b-2 border-primary"></div>
            </div>
          ) : !sortedTenants?.length ? (
            <div className="text-center py-8 text-muted-foreground">
              <p className="text-sm font-medium">No tenants yet</p>
              <p className="text-xs mt-1">Create your first tenant to get started</p>
            </div>
          ) : (
            <div className="border rounded-lg">
              <table className="w-full">
                <thead>
                  <tr className="border-b bg-muted/50">
                    <SortableHeader
                      direction={getSortDirection("id")}
                      onSort={() => requestSort("id")}
                    >
                      ID
                    </SortableHeader>
                    <SortableHeader
                      direction={getSortDirection("state")}
                      onSort={() => requestSort("state")}
                    >
                      Status
                    </SortableHeader>
                    <SortableHeader
                      direction={getSortDirection("created_at")}
                      onSort={() => requestSort("created_at")}
                    >
                      Created
                    </SortableHeader>
                    <SortableHeader
                      direction={getSortDirection("tags")}
                      onSort={() => requestSort("tags")}
                    >
                      Tags
                    </SortableHeader>
                    <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">
                      Connection
                    </th>
                    <th className="text-right px-3 py-2 text-xs font-medium text-muted-foreground">
                      Actions
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {sortedTenants.map((tenant) => (
                    <tr key={tenant.id} className="border-b last:border-0 hover:bg-muted/50">
                      <td className="px-3 py-2.5">
                        <Link
                          to={`/tenants/${tenant.id}`}
                          className="text-sm font-medium hover:underline font-mono"
                        >
                          t{tenant.id}
                        </Link>
                      </td>
                      <td className="px-3 py-2.5">
                        <span
                          className={cn(
                            "inline-flex items-center gap-1.5 text-xs",
                            tenant.state === "ENABLED"
                              ? "text-green-600"
                              : "text-red-600"
                          )}
                        >
                          <span
                            className={cn(
                              "w-1.5 h-1.5 rounded-full",
                              tenant.state === "ENABLED"
                                ? "bg-green-500"
                                : "bg-red-500"
                            )}
                          />
                          {tenant.state}
                        </span>
                      </td>
                      <td className="px-3 py-2.5">
                        <span className="text-xs text-muted-foreground">
                          {tenant.created_at
                            ? new Date(tenant.created_at.endsWith("Z") ? tenant.created_at : tenant.created_at + "Z").toLocaleString()
                            : "-"}
                        </span>
                      </td>
                      <td className="px-3 py-2.5">
                        {tenant.tags && tenant.tags.length > 0 ? (
                          <div className="flex flex-wrap gap-1">
                            {tenant.tags.map((tag) => (
                              <span
                                key={tag}
                                className="inline-flex items-center px-1.5 py-0.5 rounded text-xs bg-blue-100 text-blue-700"
                              >
                                {tag}
                              </span>
                            ))}
                          </div>
                        ) : (
                          <span className="text-xs text-muted-foreground">-</span>
                        )}
                      </td>
                      <td className="px-3 py-2.5">
                        {tenant.endpoints && tenant.endpoints.length > 0 ? (
                          <div className="flex flex-col gap-0.5">
                            <code className="text-xs bg-muted px-2 py-0.5 rounded">
                              {tenant.endpoints[0].host}:{tenant.endpoints[0].port}
                            </code>
                            {tenant.endpoints.length > 1 && (
                              <span className="text-xs text-muted-foreground">
                                +{tenant.endpoints.length - 1} more
                              </span>
                            )}
                          </div>
                        ) : (
                          <span className="text-xs text-muted-foreground">-</span>
                        )}
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex justify-end gap-1">
                          <Button
                            variant="ghost"
                            size="sm"
                            asChild
                            className="h-7 text-xs"
                          >
                            <Link to={`/tenants/${tenant.id}`}>
                              <Users className="w-3.5 h-3.5 mr-1" />
                              Users
                            </Link>
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 w-7 p-0"
                            onClick={() => setEditingTenant(tenant)}
                          >
                            <Edit className="w-3.5 h-3.5" />
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-7 w-7 p-0 text-destructive hover:text-destructive"
                            onClick={() => setConfirmRemove(tenant.id)}
                          >
                            <Ban className="w-3.5 h-3.5" />
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
