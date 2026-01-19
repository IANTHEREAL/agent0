import { useState, useCallback } from "react"
import { useConnectTenant } from "@/api/tenants"

interface TenantSessionState {
  isConnected: boolean
  sessionId: string | null
  expiresAt: string | null
}

function sessionKey(tenantId: string) {
  return `tenant_session:${tenantId}`
}

export function useTenantSession(tenantId: string) {
  const [state, setState] = useState<TenantSessionState>(() => {
    const sessionId = sessionStorage.getItem(sessionKey(tenantId))
    return {
      isConnected: !!sessionId,
      sessionId,
      expiresAt: null,
    }
  })

  const connectMutation = useConnectTenant(tenantId)

  const connect = useCallback(
    async (adminUser: string, adminPassword: string) => {
      const result = await connectMutation.mutateAsync({
        admin_user: adminUser,
        admin_password: adminPassword,
      })

      sessionStorage.setItem(sessionKey(tenantId), result.session_id)

      setState({
        isConnected: true,
        sessionId: result.session_id,
        expiresAt: result.expires_at,
      })

      return result
    },
    [connectMutation, tenantId]
  )

  const disconnect = useCallback(() => {
    sessionStorage.removeItem(sessionKey(tenantId))
    setState({
      isConnected: false,
      sessionId: null,
      expiresAt: null,
    })
  }, [tenantId])

  return {
    ...state,
    connect,
    disconnect,
    isConnecting: connectMutation.isPending,
    connectError: connectMutation.error,
  }
}
