import { Routes, Route, Navigate } from 'react-router-dom'
import { Toaster } from '@/components/ui/toaster'
import { AppLayout } from '@/components/layout/AppLayout'
import { TenantsPage } from '@/pages/TenantsPage'
import { TenantDetailPage } from '@/pages/TenantDetailPage'

function App() {
  return (
    <>
      <Routes>
        <Route element={<AppLayout />}>
          <Route path="/" element={<Navigate to="/tenants" replace />} />
          <Route path="/tenants" element={<TenantsPage />} />
          <Route path="/tenants/:name" element={<TenantDetailPage />} />
        </Route>
      </Routes>
      <Toaster />
    </>
  )
}

export default App
