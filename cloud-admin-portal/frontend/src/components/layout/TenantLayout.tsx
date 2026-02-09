import { Outlet, Link, useParams, useLocation } from "react-router-dom"
import { ArrowLeft, LayoutDashboard, Terminal } from "lucide-react"
import { cn } from "@/lib/utils"
import { useTenant } from "@/api/tenants"

export function TenantLayout() {
  const { id: tenantId } = useParams<{ id: string }>()
  const location = useLocation()
  const { data: tenant, isLoading } = useTenant(tenantId!)

  const navItems = [
    { path: "", label: "Overview", icon: LayoutDashboard },
    { path: "/sql", label: "SQL Editor", icon: Terminal },
  ]

  const isActive = (itemPath: string) => {
    const fullPath = `/tenants/${tenantId}${itemPath}`
    if (itemPath === "") {
      return location.pathname === `/tenants/${tenantId}` || location.pathname === `/tenants/${tenantId}/`
    }
    return location.pathname.startsWith(fullPath)
  }

  if (isLoading) {
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
        <Link to="/tenants" className="text-primary hover:underline text-sm mt-2 inline-block">
          Back to tenants
        </Link>
      </div>
    )
  }

  return (
    <div className="flex h-full -m-5">
      <aside className="w-48 border-r border-border flex flex-col bg-muted/20">
        <div className="p-3 border-b border-border">
          <Link
            to="/tenants"
            className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors mb-2"
          >
            <ArrowLeft className="w-3 h-3" />
            Back to Tenants
          </Link>
          <h2 className="text-base font-semibold font-mono">{tenantId}</h2>
          <span
            className={cn(
              "inline-flex items-center gap-1 px-1.5 py-0.5 rounded-full text-[10px] mt-1 w-fit",
              tenant.state === "ACTIVE"
                ? "bg-green-500/10 text-green-600"
                : tenant.state === "CREATING" || tenant.state === "DISABLING"
                  ? "bg-amber-500/10 text-amber-600"
                  : "bg-red-500/10 text-red-600"
            )}
          >
            <span
              className={cn(
                "w-1 h-1 rounded-full",
                tenant.state === "ACTIVE"
                  ? "bg-green-500"
                  : tenant.state === "CREATING" || tenant.state === "DISABLING"
                    ? "bg-amber-500"
                    : "bg-red-500"
              )}
            />
            {tenant.state}
          </span>
        </div>

        <nav className="flex-1 p-2">
          <ul className="space-y-0.5">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={`/tenants/${tenantId}${item.path}`}
                  className={cn(
                    "flex items-center gap-2 px-2.5 py-1.5 rounded-md text-xs font-medium transition-colors",
                    isActive(item.path)
                      ? "bg-secondary text-foreground"
                      : "text-muted-foreground hover:bg-secondary/50 hover:text-foreground"
                  )}
                >
                  <item.icon className="w-3.5 h-3.5" />
                  {item.label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>
      </aside>

      <main className="flex-1 overflow-auto p-5">
        <Outlet />
      </main>
    </div>
  )
}
