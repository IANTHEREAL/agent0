import { Activity, AlertTriangle, Clock, Gauge, Users } from "lucide-react"
import { useTenantObservability } from "@/api/tenants"
import { ApiError } from "@/api/client"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { cn } from "@/lib/utils"

type Props = {
  tenantId: string
}

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
  const { data, isLoading, error } = useTenantObservability(tenantId)
  const apiError = error instanceof ApiError ? error : null

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
          <div className="flex items-center gap-2 text-xs text-muted-foreground rounded-lg border border-border/50 bg-muted/20 px-3 py-2">
            <AlertTriangle className="h-4 w-4" />
            {apiError?.status === 409
              ? "Observability account is not bootstrapped for this tenant"
              : "Failed to load metrics"}
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
              {data.samples.length === 0 ? (
                <div className="px-4 py-4 text-xs text-muted-foreground">
                  No samples yet. Slow queries and errors are sampled first.
                </div>
              ) : (
                <table className="w-full">
                  <thead>
                    <tr className="border-b bg-muted/10">
                      <th className="text-left px-4 py-2 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                        Query
                      </th>
                      <th className="text-right px-3 py-2 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                        Count
                      </th>
                      <th className="text-right px-3 py-2 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                        p99
                      </th>
                      <th className="text-right px-3 py-2 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                        avg
                      </th>
                      <th className="text-right px-4 py-2 text-[11px] font-semibold text-muted-foreground uppercase tracking-wider">
                        Last Seen
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.samples.map((s, idx) => (
                      <tr
                        key={`${idx}-${s.query.slice(0, 32)}`}
                        className={cn(
                          "hover:bg-muted/40 transition-colors",
                          idx !== data.samples.length - 1 && "border-b border-border/30"
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
