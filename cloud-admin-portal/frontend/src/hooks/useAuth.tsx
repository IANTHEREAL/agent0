/**
 * Authentication context and hook
 */

import { createContext, useContext, useState, useEffect, ReactNode } from "react"
import { useNavigate } from "react-router-dom"
import { apiRequest } from "@/api/client"

interface AuthContextType {
  isAuthenticated: boolean
  isLoading: boolean
  login: (password: string) => Promise<void>
  logout: () => void
}

const AuthContext = createContext<AuthContextType | null>(null)

export function AuthProvider({ children }: { children: ReactNode }) {
  const [isAuthenticated, setIsAuthenticated] = useState(false)
  const [isLoading, setIsLoading] = useState(true)
  const navigate = useNavigate()

  useEffect(() => {
    const token = localStorage.getItem("auth_token")
    if (token) {
      // Validate token by calling /api/auth/me
      apiRequest("/auth/me")
        .then(() => setIsAuthenticated(true))
        .catch(() => {
          localStorage.removeItem("auth_token")
          setIsAuthenticated(false)
        })
        .finally(() => setIsLoading(false))
    } else {
      setIsLoading(false)
    }
  }, [])

  const login = async (password: string) => {
    const response = await apiRequest<{ token: string }>("/auth/login", {
      method: "POST",
      body: JSON.stringify({ password }),
    })
    localStorage.setItem("auth_token", response.token)
    setIsAuthenticated(true)
    navigate("/tenants")
  }

  const logout = () => {
    localStorage.removeItem("auth_token")
    sessionStorage.removeItem("tenant_session")
    setIsAuthenticated(false)
    navigate("/login")
  }

  return (
    <AuthContext.Provider value={{ isAuthenticated, isLoading, login, logout }}>
      {children}
    </AuthContext.Provider>
  )
}

export function useAuth() {
  const context = useContext(AuthContext)
  if (!context) {
    throw new Error("useAuth must be used within AuthProvider")
  }
  return context
}
