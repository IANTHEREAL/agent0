/**
 * Edit tenant metadata dialog
 */

import { useState, useEffect } from "react"
import { useUpdateTenant } from "@/api/tenants"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { useToast } from "@/components/ui/use-toast"
import { ApiError } from "@/api/client"
import type { Tenant } from "@/types"

interface EditTenantMetadataDialogProps {
  tenant: Tenant | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function EditTenantMetadataDialog({
  tenant,
  open,
  onOpenChange,
}: EditTenantMetadataDialogProps) {
  const [notes, setNotes] = useState("")
  const [tags, setTags] = useState("")

  const mutation = useUpdateTenant(tenant?.id || "")
  const { toast } = useToast()

  // Update form when tenant changes
  useEffect(() => {
    if (tenant) {
      setNotes(tenant.notes || "")
      setTags(tenant.tags?.join(", ") || "")
    }
  }, [tenant])

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()

    if (!tenant) return

    try {
      await mutation.mutateAsync({
        notes: notes.trim() || null,
        tags: tags.trim() ? tags.split(",").map((t) => t.trim()).filter(Boolean) : null,
      })

      toast({
        title: "Tenant Updated",
        description: "Tenant metadata has been updated successfully.",
      })

      onOpenChange(false)
    } catch (error) {
      const message = error instanceof ApiError ? error.message : "Failed to update tenant"
      toast({
        title: "Error",
        description: message,
        variant: "destructive",
      })
    }
  }

  const handleCancel = () => {
    // Reset form to original values
    if (tenant) {
      setNotes(tenant.notes || "")
      setTags(tenant.tags?.join(", ") || "")
    }
    onOpenChange(false)
  }

  if (!tenant) return null

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Edit Tenant Metadata</DialogTitle>
          <DialogDescription>
            Update notes and tags for tenant "{tenant.id}"
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="notes">Notes</Label>
            <textarea
              id="notes"
              className="w-full p-2 border rounded-md min-h-[100px] text-sm"
              value={notes}
              onChange={(e) => setNotes(e.target.value)}
              placeholder="Optional notes about this tenant..."
            />
            <p className="text-xs text-muted-foreground">
              Add any relevant information about this tenant
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="tags">Tags</Label>
            <Input
              id="tags"
              value={tags}
              onChange={(e) => setTags(e.target.value)}
              placeholder="production, us-west, customer-abc"
            />
            <p className="text-xs text-muted-foreground">
              Comma-separated tags for categorization and filtering
            </p>
          </div>

          {tenant.created_at && (
            <div className="text-xs text-muted-foreground pt-2 border-t">
              <p>Created: {new Date(tenant.created_at).toLocaleString()}</p>
              {tenant.created_by && <p>Created by: {tenant.created_by}</p>}
              {tenant.updated_at && (
                <p>Last updated: {new Date(tenant.updated_at).toLocaleString()}</p>
              )}
            </div>
          )}

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={handleCancel}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending ? "Saving..." : "Save Changes"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
