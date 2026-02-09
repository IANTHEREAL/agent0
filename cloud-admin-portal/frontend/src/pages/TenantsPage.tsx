import { useState, useEffect, useCallback } from "react"
import { Link, useSearchParams } from "react-router-dom"
import { Plus, Ban, Users, Edit, Search, ChevronLeft, ChevronRight, ChevronsLeft, ChevronsRight } from "lucide-react"
import { useTenants, useRemoveTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import { CreateTenantDialog } from "@/components/tenants/CreateTenantDialog"
import { EditTenantMetadataDialog } from "@/components/tenants/EditTenantMetadataDialog"
import { ConfirmDialog } from "@/components/common/ConfirmDialog"
import type { Tenant } from "@/types"

const PAGE_SIZE = 20

export function TenantsPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const page = Number(searchParams.get("page")) || 1
  const search = searchParams.get("q") || ""

  const [searchInput, setSearchInput] = useState(search)
  const [showCreateDialog, setShowCreateDialog] = useState(false)
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null)
  const [editingTenant, setEditingTenant] = useState<Tenant | null>(null)

  const { data, isLoading, isFetching } = useTenants({
    page,
    size: PAGE_SIZE,
    q: search || undefined,
  })

  const tenants = data?.items ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  const removeMutation = useRemoveTenant()
  const { toast } = useToast()

  const setPage = useCallback((p: number) => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev)
      if (p <= 1) next.delete("page")
      else next.set("page", String(p))
      return next
    })
  }, [setSearchParams])

  const setSearch = useCallback((q: string) => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev)
      if (q) next.set("q", q)
      else next.delete("q")
      next.delete("page")
      return next
    })
  }, [setSearchParams])

  useEffect(() => {
    const timer = setTimeout(() => {
      if (searchInput !== search) setSearch(searchInput)
    }, 300)
    return () => clearTimeout(timer)
  }, [searchInput, search, setSearch])

  const handleRemove = async (tenantId: string) => {
    try {
      await removeMutation.mutateAsync(tenantId)
      toast({
        title: "Tenant Removed",
        description: `Tenant "${tenantId}" has been removed from the portal.`,
      })
    } catch {
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
          <div className="flex items-center justify-between">
            <div>
              <CardTitle className="text-base">All Tenants</CardTitle>
              <CardDescription className="text-xs">
                {total > 0 ? `${total.toLocaleString()} tenant${total !== 1 ? "s" : ""}` : "View and manage tenant keyspaces"}
              </CardDescription>
            </div>
            <div className="relative w-64">
              <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground" />
              <Input
                placeholder="Search by ID..."
                value={searchInput}
                onChange={(e) => setSearchInput(e.target.value)}
                className="h-8 pl-8 text-sm"
              />
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="flex items-center justify-center py-8">
              <div className="animate-spin rounded-full h-6 w-6 border-b-2 border-primary"></div>
            </div>
          ) : !tenants.length ? (
            <div className="text-center py-8 text-muted-foreground">
              <p className="text-sm font-medium">{search ? "No tenants match your search" : "No tenants yet"}</p>
              <p className="text-xs mt-1">{search ? "Try a different search term" : "Create your first tenant to get started"}</p>
            </div>
          ) : (
            <>
              <div className={cn("border rounded-lg", isFetching && "opacity-60 transition-opacity")}>
                <table className="w-full">
                  <thead>
                    <tr className="border-b bg-muted/50">
                      <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">ID</th>
                      <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Status</th>
                      <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Created</th>
                      <th className="text-left px-3 py-2 text-xs font-medium text-muted-foreground">Tags</th>
                      <th className="text-right px-3 py-2 text-xs font-medium text-muted-foreground">Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    {tenants.map((tenant) => (
                      <tr key={tenant.id} className="border-b last:border-0 hover:bg-muted/50">
                        <td className="px-3 py-2.5">
                          <Link
                            to={`/tenants/${tenant.id}`}
                            className="text-sm font-medium hover:underline font-mono"
                          >
                            {tenant.id}
                          </Link>
                        </td>
                        <td className="px-3 py-2.5">
                          <span
                            className={cn(
                              "inline-flex items-center gap-1.5 text-xs",
                              tenant.state === "ACTIVE"
                                ? "text-green-600"
                                : tenant.state === "CREATING" || tenant.state === "DISABLING"
                                  ? "text-yellow-600"
                                  : "text-red-600"
                            )}
                          >
                            <span
                              className={cn(
                                "w-1.5 h-1.5 rounded-full",
                                tenant.state === "ACTIVE"
                                  ? "bg-green-500"
                                  : tenant.state === "CREATING" || tenant.state === "DISABLING"
                                    ? "bg-yellow-500"
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
                          <div className="flex justify-end gap-1">
                            <Button variant="ghost" size="sm" asChild className="h-7 text-xs">
                              <Link to={`/tenants/${tenant.id}`}>
                                <Users className="w-3.5 h-3.5 mr-1" />
                                Manage
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

              {totalPages > 1 && (
                <div className="flex items-center justify-between pt-4">
                  <span className="text-xs text-muted-foreground">
                    {((page - 1) * PAGE_SIZE + 1).toLocaleString()}–{Math.min(page * PAGE_SIZE, total).toLocaleString()} of {total.toLocaleString()}
                  </span>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-7 w-7 p-0"
                      disabled={page <= 1}
                      onClick={() => setPage(1)}
                    >
                      <ChevronsLeft className="h-3.5 w-3.5" />
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-7 w-7 p-0"
                      disabled={page <= 1}
                      onClick={() => setPage(page - 1)}
                    >
                      <ChevronLeft className="h-3.5 w-3.5" />
                    </Button>
                    <span className="px-2 text-xs text-muted-foreground">
                      Page {page} of {totalPages.toLocaleString()}
                    </span>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-7 w-7 p-0"
                      disabled={page >= totalPages}
                      onClick={() => setPage(page + 1)}
                    >
                      <ChevronRight className="h-3.5 w-3.5" />
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-7 w-7 p-0"
                      disabled={page >= totalPages}
                      onClick={() => setPage(totalPages)}
                    >
                      <ChevronsRight className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>
              )}
            </>
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
