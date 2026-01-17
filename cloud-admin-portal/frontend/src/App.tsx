import { Routes, Route, Navigate } from 'react-router-dom'
import { AuthProvider } from '@/hooks/useAuth'
import { Toaster } from '@/components/ui/toaster'
import { ProtectedRoute } from '@/components/layout/ProtectedRoute'
import { AppLayout } from '@/components/layout/AppLayout'
import { LoginPage } from '@/pages/LoginPage'
import { TenantsPage } from '@/pages/TenantsPage'
import { TenantDetailPage } from '@/pages/TenantDetailPage'

function App() {
  return (
    <AuthProvider>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route element={<ProtectedRoute />}>
          <Route element={<AppLayout />}>
            <Route path="/" element={<Navigate to="/tenants" replace />} />
            <Route path="/tenants" element={<TenantsPage />} />
            <Route path="/tenants/:name" element={<TenantDetailPage />} />
          </Route>
        </Route>
      </Routes>
      <Toaster />
    </AuthProvider>
  )
}

export default App
