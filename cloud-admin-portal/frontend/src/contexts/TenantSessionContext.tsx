import { createContext, useContext, useState, useCallback, useEffect, type ReactNode } from "react"
import { useConnectTenant } from "@/api/tenants"
import { ApiError } from "@/api/client"

interface TenantSessionContextValue {
  isConnected: boolean
  sessionId: string | null
  adminUser: string
  adminPassword: string
  connect: (user: string, password: string) => Promise<void>
  disconnect: () => void
  isConnecting: boolean
  setAdminUser: (user: string) => void
  setAdminPassword: (password: string) => void
}

const TenantSessionContext = createContext<TenantSessionContextValue | null>(null)

function skey(tenantId: string, suffix: string) {
  return `tenant_session:${tenantId}:${suffix}`
}

export function TenantSessionProvider({ tenantId, children }: { tenantId: string; children: ReactNode }) {
  const [sessionId, setSessionId] = useState<string | null>(() =>
    sessionStorage.getItem(skey(tenantId, "id"))
  )
  const [adminUser, setAdminUser] = useState(() =>
    sessionStorage.getItem(skey(tenantId, "user")) || "admin"
  )
  const [adminPassword, setAdminPassword] = useState(() =>
    sessionStorage.getItem(skey(tenantId, "pass")) || ""
  )

  const connectMutation = useConnectTenant(tenantId)

  const disconnect = useCallback(() => {
    sessionStorage.removeItem(skey(tenantId, "id"))
    sessionStorage.removeItem(skey(tenantId, "user"))
    sessionStorage.removeItem(skey(tenantId, "pass"))
    // keep legacy key clean
    sessionStorage.removeItem(`tenant_session:${tenantId}`)
    setSessionId(null)
    setAdminPassword("")
  }, [tenantId])

  const connect = useCallback(async (user: string, password: string) => {
    try {
      const result = await connectMutation.mutateAsync({
        admin_user: user,
        admin_password: password,
      })
      // Store in both new keyed format and legacy format (for apiRequest header injection)
      sessionStorage.setItem(skey(tenantId, "id"), result.session_id)
      sessionStorage.setItem(skey(tenantId, "user"), user)
      sessionStorage.setItem(skey(tenantId, "pass"), password)
      sessionStorage.setItem(`tenant_session:${tenantId}`, result.session_id)
      setSessionId(result.session_id)
      setAdminUser(user)
      setAdminPassword(password)
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        disconnect()
      }
      throw err
    }
  }, [connectMutation, tenantId, disconnect])

  // Listen for 401 events dispatched by the API client
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail
      if (detail?.tenantId === tenantId) {
        disconnect()
      }
    }
    window.addEventListener("session-expired", handler)
    return () => window.removeEventListener("session-expired", handler)
  }, [tenantId, disconnect])

  return (
    <TenantSessionContext.Provider value={{
      isConnected: !!sessionId,
      sessionId,
      adminUser,
      adminPassword,
      connect,
      disconnect,
      isConnecting: connectMutation.isPending,
      setAdminUser,
      setAdminPassword,
    }}>
      {children}
    </TenantSessionContext.Provider>
  )
}

export function useTenantSessionContext() {
  const ctx = useContext(TenantSessionContext)
  if (!ctx) throw new Error("useTenantSessionContext must be used within TenantSessionProvider")
  return ctx
}
