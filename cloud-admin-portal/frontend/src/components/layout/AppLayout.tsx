import { Outlet, Link, useLocation } from "react-router-dom"
import { useQuery } from "@tanstack/react-query"
import { Database, Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { apiRequest } from "@/api/client"
import type { HealthResponse } from "@/types"

export function AppLayout() {
  const location = useLocation()

  const { data: health } = useQuery({
    queryKey: ["health"],
    queryFn: () => apiRequest<HealthResponse>("/health"),
    refetchInterval: 30000,
  })

  const navItems = [
    { path: "/tenants", label: "Tenants", icon: Layers },
  ]

  return (
    <div className="flex h-screen bg-background">
      <aside className="w-64 border-r border-border flex flex-col">
        <div className="p-6 border-b border-border">
          <Link to="/" className="flex items-center gap-3">
            <div className="w-8 h-8 bg-primary rounded-lg flex items-center justify-center">
              <Database className="w-5 h-5 text-primary-foreground" />
            </div>
            <span className="font-semibold text-lg">pg-tikv</span>
          </Link>
        </div>

        <nav className="flex-1 p-4">
          <ul className="space-y-1">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={cn(
                    "flex items-center gap-3 px-3 py-2 rounded-lg text-sm font-medium transition-colors",
                    location.pathname.startsWith(item.path)
                      ? "bg-secondary text-foreground"
                      : "text-muted-foreground hover:bg-secondary/50 hover:text-foreground"
                  )}
                >
                  <item.icon className="w-4 h-4" />
                  {item.label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>

        <div className="p-4 border-t border-border text-xs text-muted-foreground">
          pg-tikv Admin Portal
        </div>
      </aside>

      <div className="flex-1 flex flex-col overflow-hidden">
        <header className="h-14 border-b border-border flex items-center justify-between px-6">
          <div />
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 text-sm">
              <div
                className={cn(
                  "w-2 h-2 rounded-full",
                  health?.pd_healthy ? "bg-green-500" : "bg-yellow-500"
                )}
              />
              <span className="text-muted-foreground">
                {health?.status === "healthy" ? "Connected" : "Degraded"}
              </span>
            </div>
          </div>
        </header>

        <main className="flex-1 overflow-auto p-6">
          <Outlet />
        </main>
      </div>
    </div>
  )
}
