/**
 * Tenant session management hook
 */

import { useState, useCallback } from "react"
import { useConnectTenant } from "@/api/tenants"

interface TenantSessionState {
  isConnected: boolean
  sessionId: string | null
  expiresAt: string | null
}

export function useTenantSession(tenantName: string) {
  const [state, setState] = useState<TenantSessionState>(() => {
    // Check for existing session
    const sessionId = sessionStorage.getItem("tenant_session")
    return {
      isConnected: !!sessionId,
      sessionId,
      expiresAt: null,
    }
  })

  const connectMutation = useConnectTenant(tenantName)

  const connect = useCallback(
    async (adminUser: string, adminPassword: string) => {
      const result = await connectMutation.mutateAsync({
        admin_user: adminUser,
        admin_password: adminPassword,
      })

      setState({
        isConnected: true,
        sessionId: result.session_id,
        expiresAt: result.expires_at,
      })

      return result
    },
    [connectMutation]
  )

  const disconnect = useCallback(() => {
    sessionStorage.removeItem("tenant_session")
    setState({
      isConnected: false,
      sessionId: null,
      expiresAt: null,
    })
  }, [])

  return {
    ...state,
    connect,
    disconnect,
    isConnecting: connectMutation.isPending,
    connectError: connectMutation.error,
  }
}
