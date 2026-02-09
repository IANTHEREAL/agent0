import { Outlet, Link } from "react-router-dom"
import { useQuery } from "@tanstack/react-query"
import { cn } from "@/lib/utils"
import { apiRequest } from "@/api/client"
import type { HealthResponse } from "@/types"
import { TiDBLogo } from "@/assets/TiDBLogo"

export function AppLayout() {
  const { data: health } = useQuery({
    queryKey: ["health"],
    queryFn: () => apiRequest<HealthResponse>("/health"),
    refetchInterval: 30000,
  })

  return (
    <div className="flex flex-col h-screen bg-background">
      <header className="h-12 border-b border-border flex items-center justify-between px-5 shrink-0">
        <Link to="/" className="flex items-center gap-2.5">
          <TiDBLogo className="w-6 h-6 flex-shrink-0" />
          <span className="font-bold text-sm leading-tight">TiDB PostgreSQL</span>
          <span className="text-[10px] text-muted-foreground ml-1">Admin</span>
        </Link>
        <div className="flex items-center gap-2 text-xs">
          <div
            className={cn(
              "w-1.5 h-1.5 rounded-full",
              health?.pd_healthy ? "bg-green-500" : "bg-yellow-500"
            )}
          />
          <span className="text-muted-foreground">
            {health?.status === "healthy" ? "Cluster Healthy" : "Cluster Degraded"}
          </span>
        </div>
      </header>

      <main className="flex-1 overflow-auto p-5">
        <Outlet />
      </main>
    </div>
  )
}
