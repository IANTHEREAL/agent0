/**
 * SQL Editor page - dedicated page for SQL query execution
 */

import { useParams } from "react-router-dom"
import { SqlEditor } from "@/components/sql/SqlEditor"

export function SqlEditorPage() {
  const { id: tenantId } = useParams<{ id: string }>()

  if (!tenantId) {
    return (
      <div className="text-center py-12">
        <p className="text-lg font-medium">Tenant not found</p>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <SqlEditor tenantId={tenantId} />
    </div>
  )
}
