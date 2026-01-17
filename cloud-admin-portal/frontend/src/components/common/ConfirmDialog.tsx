/**
 * ConfirmDialog - Reusable confirmation dialog component
 *
 * Features:
 * - Customizable confirmation message
 * - Optional text input confirmation (type to confirm)
 * - Loading state support
 * - Destructive and default variants
 */

import { useState } from "react"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"

export interface ConfirmDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description: string | React.ReactNode
  confirmLabel?: string
  cancelLabel?: string
  variant?: "default" | "destructive"
  onConfirm: () => void | Promise<void>
  requireTyping?: {
    matchText: string
    placeholder: string
  }
}

export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel = "Confirm",
  cancelLabel = "Cancel",
  variant = "default",
  onConfirm,
  requireTyping,
}: ConfirmDialogProps) {
  const [confirmText, setConfirmText] = useState("")
  const [isLoading, setIsLoading] = useState(false)

  const canConfirm = requireTyping
    ? confirmText === requireTyping.matchText
    : true

  const handleConfirm = async () => {
    setIsLoading(true)
    try {
      await onConfirm()
      onOpenChange(false)
      setConfirmText("")
    } catch (error) {
      // Error will be handled by the parent component
    } finally {
      setIsLoading(false)
    }
  }

  const handleCancel = () => {
    setConfirmText("")
    onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription asChild>
            <div>{description}</div>
          </DialogDescription>
        </DialogHeader>

        {requireTyping && (
          <div className="space-y-2">
            <p className="text-sm text-muted-foreground">
              Type <strong>{requireTyping.matchText}</strong> to confirm:
            </p>
            <Input
              value={confirmText}
              onChange={(e) => setConfirmText(e.target.value)}
              placeholder={requireTyping.placeholder}
            />
          </div>
        )}

        <DialogFooter>
          <Button
            variant="outline"
            onClick={handleCancel}
            disabled={isLoading}
          >
            {cancelLabel}
          </Button>
          <Button
            variant={variant}
            onClick={handleConfirm}
            disabled={!canConfirm || isLoading}
          >
            {isLoading ? "Processing..." : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
