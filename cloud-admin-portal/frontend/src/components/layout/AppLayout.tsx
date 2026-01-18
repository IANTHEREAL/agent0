import { Outlet, Link, useLocation } from "react-router-dom"
import { useQuery } from "@tanstack/react-query"
import { Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { apiRequest } from "@/api/client"
import type { HealthResponse } from "@/types"
import { TiDBLogo } from "@/assets/TiDBLogo"

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
      <aside className="w-56 border-r border-border flex flex-col">
        <div className="p-4 border-b border-border">
          <Link to="/" className="flex items-center gap-2.5">
            <TiDBLogo className="w-7 h-7 flex-shrink-0" />
            <div className="flex flex-col">
              <span className="font-bold text-base leading-tight">TiDB</span>
              <span className="text-[10px] text-muted-foreground font-normal">PostgreSQL</span>
            </div>
          </Link>
        </div>

        <nav className="flex-1 p-3">
          <ul className="space-y-0.5">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={cn(
                    "flex items-center gap-2.5 px-2.5 py-1.5 rounded-md text-xs font-medium transition-colors",
                    location.pathname.startsWith(item.path)
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

        <div className="p-3 border-t border-border text-[10px] text-muted-foreground">
          Admin Portal
        </div>
      </aside>

      <div className="flex-1 flex flex-col overflow-hidden">
        <header className="h-12 border-b border-border flex items-center justify-between px-5">
          <div />
          <div className="flex items-center gap-3">
            <div className="flex items-center gap-2 text-xs">
              <div
                className={cn(
                  "w-1.5 h-1.5 rounded-full",
                  health?.pd_healthy ? "bg-green-500" : "bg-yellow-500"
                )}
              />
              <span className="text-muted-foreground">
                {health?.status === "healthy" ? "Connected" : "Degraded"}
              </span>
            </div>
          </div>
        </header>

        <main className="flex-1 overflow-auto p-5">
          <Outlet />
        </main>
      </div>
    </div>
  )
}
