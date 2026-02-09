import { useState } from "react"
import { Activity, AlertTriangle, Clock, Gauge, Loader2, Users, Zap, Lock, LogIn } from "lucide-react"
import { useTenantObservability, bootstrapTenantObservabilityUser } from "@/api/tenants"
import { ApiError } from "@/api/client"
import { useTenantSessionContext } from "@/contexts/TenantSessionContext"
import { useSortableData } from "@/hooks/useSortableData"
import { useQueryClient } from "@tanstack/react-query"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { SortableHeader } from "@/components/ui/sortable-header"
import { useToast } from "@/components/ui/use-toast"
import { cn } from "@/lib/utils"
import type { QuerySample } from "@/types"

type Props = {
  tenantId: string
}

type SampleSortKey = "query" | "sample_count" | "latency_p99_ms" | "latency_avg_ms" | "last_seen_ms_ago"

const sampleSortColumns = [
  { key: "query" as const, getValue: (s: QuerySample) => s.query },
  { key: "sample_count" as const, getValue: (s: QuerySample) => s.sample_count },
  { key: "latency_p99_ms" as const, getValue: (s: QuerySample) => s.latency_p99_ms },
  { key: "latency_avg_ms" as const, getValue: (s: QuerySample) => s.latency_avg_ms },
  { key: "last_seen_ms_ago" as const, getValue: (s: QuerySample) => s.last_seen_ms_ago },
]

function formatNumber(n: number) {
  return Number.isFinite(n) ? n.toLocaleString() : "-"
}

function formatRate(n: number) {
  if (!Number.isFinite(n)) return "-"
  if (n >= 1000) return n.toFixed(0)
  if (n >= 10) return n.toFixed(1)
  return n.toFixed(2)
}

function formatMs(n: number) {
  if (!Number.isFinite(n)) return "-"
  if (n >= 1000) return `${(n / 1000).toFixed(2)}s`
  if (n >= 10) return `${n.toFixed(1)}ms`
  return `${n.toFixed(2)}ms`
}

function formatAge(ms: number) {
  if (!Number.isFinite(ms)) return "-"
  if (ms < 1000) return `${Math.round(ms)}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`
  if (ms < 60 * 60_000) return `${Math.round(ms / 60_000)}m`
  return `${Math.round(ms / (60 * 60_000))}h`
}

export function TenantObservabilityCard({ tenantId }: Props) {
  const { isConnected, adminUser, adminPassword, connect, isConnecting, setAdminUser, setAdminPassword } = useTenantSessionContext()
  const { toast } = useToast()
  const { data, isLoading, error } = useTenantObservability(tenantId)
  const apiError = error instanceof ApiError ? error : null
  const queryClient = useQueryClient()
  const [bootstrapping, setBootstrapping] = useState(false)
  const [showBootstrapLogin, setShowBootstrapLogin] = useState(false)

  const { sortedData: sortedSamples, requestSort, getSortDirection } = useSortableData<QuerySample, SampleSortKey>(
    data?.samples,
    sampleSortColumns
  )

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center gap-2">
          <Activity className="h-4 w-4 text-muted-foreground" />
          <CardTitle className="text-base">Observability</CardTitle>
        </div>
        <CardDescription className="text-xs">
          Rolling 1-hour metrics (auto refresh every 5s)
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <div className="flex items-center justify-center py-6 text-xs text-muted-foreground">
            Loading metrics...
          </div>
        ) : error || !data ? (
          <div className="space-y-3">
            <div className="flex items-center justify-between text-xs text-muted-foreground rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
              <div className="flex items-center gap-2">
                <AlertTriangle className="h-4 w-4" />
                {apiError?.status === 409
                  ? "Observability account is not bootstrapped for this tenant"
                  : "Failed to load metrics"}
              </div>
              {apiError?.status === 409 && isConnected && (
                <Button
                  size="sm"
                  variant="outline"
                  className="h-7 text-xs gap-1.5"
                  disabled={bootstrapping}
                  onClick={async () => {
                    setBootstrapping(true)
                    try {
                      await bootstrapTenantObservabilityUser(tenantId, adminUser, adminPassword)
                      queryClient.invalidateQueries({ queryKey: ["tenants", tenantId, "observability"] })
                    } catch {
                      toast({ title: "Bootstrap failed", description: "Could not bootstrap observability account", variant: "destructive" })
                    } finally {
                      setBootstrapping(false)
                    }
                  }}
                >
                  {bootstrapping ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : <Zap className="w-3.5 h-3.5" />}
                  Bootstrap
                </Button>
              )}
              {apiError?.status === 409 && !isConnected && !showBootstrapLogin && (
                <Button
                  size="sm"
                  variant="outline"
                  className="h-7 text-xs gap-1.5"
                  onClick={() => setShowBootstrapLogin(true)}
                >
                  <LogIn className="w-3.5 h-3.5" />
                  Connect to Bootstrap
                </Button>
              )}
            </div>
            {apiError?.status === 409 && !isConnected && showBootstrapLogin && (
              <form
                className="rounded-lg border border-border/50 bg-muted/10 p-4 space-y-3"
                onSubmit={async (e) => {
                  e.preventDefault()
                  try {
                    await connect(adminUser, adminPassword)
                    toast({ title: "Connected", description: "Now click Bootstrap to set up observability" })
                  } catch {
                    toast({ title: "Connection Failed", description: "Invalid credentials", variant: "destructive" })
                  }
                }}
              >
                <div className="flex items-center gap-2 text-xs text-muted-foreground">
                  <Lock className="w-3.5 h-3.5" />
                  Connect with admin credentials to bootstrap observability
                </div>
                <div className="grid grid-cols-2 gap-3">
                  <div className="space-y-1">
                    <Label htmlFor="obs_user" className="text-xs">Username</Label>
                    <Input id="obs_user" className="h-8 text-sm" value={adminUser} onChange={(e) => setAdminUser(e.target.value)} placeholder="admin" />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="obs_pass" className="text-xs">Password</Label>
                    <Input id="obs_pass" type="password" className="h-8 text-sm" value={adminPassword} onChange={(e) => setAdminPassword(e.target.value)} placeholder="••••••••" />
                  </div>
                </div>
                <Button type="submit" size="sm" className="h-8 w-full gap-2" disabled={isConnecting}>
                  {isConnecting ? <><Loader2 className="w-3.5 h-3.5 animate-spin" /> Connecting...</> : <><LogIn className="w-3.5 h-3.5" /> Connect</>}
                </Button>
              </form>
            )}
          </div>
        ) : (
          <>
            <div className="grid grid-cols-2 md:grid-cols-5 gap-3">
              <div className="rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
                <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                  <Gauge className="h-3.5 w-3.5" />
                  QPS
                </div>
                <div className="mt-1 text-lg font-semibold">{formatRate(data.summary.qps)}</div>
              </div>
              <div className="rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
                <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                  <Gauge className="h-3.5 w-3.5" />
                  TPS
                </div>
                <div className="mt-1 text-lg font-semibold">{formatRate(data.summary.tps)}</div>
              </div>
              <div className="rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
                <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                  <Clock className="h-3.5 w-3.5" />
                  p99
                </div>
                <div className="mt-1 text-lg font-semibold">
                  {formatMs(data.summary.latency_p99_ms)}
                </div>
              </div>
              <div className="rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
                <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                  <Clock className="h-3.5 w-3.5" />
                  avg
                </div>
                <div className="mt-1 text-lg font-semibold">
                  {formatMs(data.summary.latency_avg_ms)}
                </div>
              </div>
              <div className="rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
                <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                  <Users className="h-3.5 w-3.5" />
                  Connections
                </div>
                <div className="mt-1 text-lg font-semibold">
                  {formatNumber(data.summary.active_connections)}
                </div>
              </div>
            </div>

            <div className="flex items-center justify-between">
              <div className="text-xs text-muted-foreground">
                Statements: {formatNumber(data.summary.statement_count)} · Commits:{" "}
                {formatNumber(data.summary.txn_commit_count)} · Errors:{" "}
                {formatNumber(data.summary.error_count)}
              </div>
              <div className="text-xs text-muted-foreground">
                Window: {formatNumber(data.summary.window_seconds)}s
              </div>
            </div>

            <div className="rounded-lg border border-border/50 overflow-hidden">
              <div className="px-4 py-2.5 border-b bg-muted/30">
                <div className="text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                  Sampled Statements (last 1h)
                </div>
              </div>
              {sortedSamples.length === 0 ? (
                <div className="px-4 py-4 text-xs text-muted-foreground">
                  No samples yet. Slow queries and errors are sampled first.
                </div>
              ) : (
                <table className="w-full">
                  <thead>
                    <tr className="border-b bg-muted/10">
                      <SortableHeader
                        direction={getSortDirection("query")}
                        onSort={() => requestSort("query")}
                        className="px-4 py-2 text-[11px] font-semibold uppercase tracking-wider"
                      >
                        Query
                      </SortableHeader>
                      <SortableHeader
                        direction={getSortDirection("sample_count")}
                        onSort={() => requestSort("sample_count")}
                        className="px-3 py-2 text-[11px] font-semibold uppercase tracking-wider"
                        align="right"
                      >
                        Count
                      </SortableHeader>
                      <SortableHeader
                        direction={getSortDirection("latency_p99_ms")}
                        onSort={() => requestSort("latency_p99_ms")}
                        className="px-3 py-2 text-[11px] font-semibold uppercase tracking-wider"
                        align="right"
                      >
                        p99
                      </SortableHeader>
                      <SortableHeader
                        direction={getSortDirection("latency_avg_ms")}
                        onSort={() => requestSort("latency_avg_ms")}
                        className="px-3 py-2 text-[11px] font-semibold uppercase tracking-wider"
                        align="right"
                      >
                        avg
                      </SortableHeader>
                      <SortableHeader
                        direction={getSortDirection("last_seen_ms_ago")}
                        onSort={() => requestSort("last_seen_ms_ago")}
                        className="px-4 py-2 text-[11px] font-semibold uppercase tracking-wider"
                        align="right"
                      >
                        Last Seen
                      </SortableHeader>
                    </tr>
                  </thead>
                  <tbody>
                    {sortedSamples.map((s, idx) => (
                      <tr
                        key={`${idx}-${s.query.slice(0, 32)}`}
                        className={cn(
                          "hover:bg-muted/40 transition-colors",
                          idx !== sortedSamples.length - 1 && "border-b border-border/30"
                        )}
                      >
                        <td className="px-4 py-2.5">
                          <code className="block max-w-[520px] truncate text-xs text-muted-foreground">
                            {s.query}
                          </code>
                          {s.error_count > 0 && (
                            <div className="mt-1 text-[10px] text-amber-600">
                              errors: {formatNumber(s.error_count)}
                            </div>
                          )}
                        </td>
                        <td className="px-3 py-2.5 text-right text-xs">
                          {formatNumber(s.sample_count)}
                        </td>
                        <td className="px-3 py-2.5 text-right text-xs">
                          {formatMs(s.latency_p99_ms)}
                        </td>
                        <td className="px-3 py-2.5 text-right text-xs">
                          {formatMs(s.latency_avg_ms)}
                        </td>
                        <td className="px-4 py-2.5 text-right text-xs text-muted-foreground">
                          {formatAge(s.last_seen_ms_ago)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>
          </>
        )}
      </CardContent>
    </Card>
  )
}
