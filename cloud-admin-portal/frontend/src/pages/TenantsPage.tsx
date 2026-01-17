/**
 * Tenants list page
 */

import { useState } from "react"
import { Link } from "react-router-dom"
import { Plus, Trash2, Users } from "lucide-react"
import { useTenants, useDeleteTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import { CreateTenantDialog } from "@/components/tenants/CreateTenantDialog"

export function TenantsPage() {
  const [showCreateDialog, setShowCreateDialog] = useState(false)
  const { data: tenants, isLoading } = useTenants()
  const deleteMutation = useDeleteTenant()
  const { toast } = useToast()

  const handleDelete = async (name: string) => {
    if (!confirm(`Are you sure you want to disable tenant "${name}"?`)) {
      return
    }

    try {
      await deleteMutation.mutateAsync(name)
      toast({
        title: "Tenant Disabled",
        description: `Tenant "${name}" has been disabled.`,
      })
    } catch (error) {
      toast({
        title: "Error",
        description: "Failed to disable tenant",
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
                            className="text-destructive hover:text-destructive"
                            onClick={() => handleDelete(tenant.name)}
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

      <CreateTenantDialog
        open={showCreateDialog}
        onOpenChange={setShowCreateDialog}
      />
    </div>
  )
}
