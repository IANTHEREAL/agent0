import { Routes, Route, Navigate } from 'react-router-dom'
import { Toaster } from '@/components/ui/toaster'
import { AppLayout } from '@/components/layout/AppLayout'
import { TenantLayout } from '@/components/layout/TenantLayout'
import { TenantsPage } from '@/pages/TenantsPage'
import { TenantDetailPage } from '@/pages/TenantDetailPage'
import { SqlEditorPage } from '@/pages/SqlEditorPage'

function App() {
  return (
    <>
      <Routes>
        <Route element={<AppLayout />}>
          <Route path="/" element={<Navigate to="/tenants" replace />} />
          <Route path="/tenants" element={<TenantsPage />} />
          <Route path="/tenants/:id" element={<TenantLayout />}>
            <Route index element={<TenantDetailPage />} />
            <Route path="sql" element={<SqlEditorPage />} />
          </Route>
        </Route>
      </Routes>
      <Toaster />
    </>
  )
}

export default App
