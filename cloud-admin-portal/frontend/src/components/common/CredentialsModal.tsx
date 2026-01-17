/**
 * CredentialsModal - Modal for displaying sensitive credentials
 *
 * Features:
 * - Displays credentials securely with show/hide toggle
 * - Copy to clipboard functionality
 * - Prevents accidental closure
 * - Requires user confirmation before closing
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
import { useToast } from "@/components/ui/use-toast"
import { Eye, EyeOff, Copy } from "lucide-react"

export interface Credential {
  label: string
  value: string
  sensitive?: boolean // Default hidden, click to show
  copyable?: boolean // Show copy button
}

export interface CredentialsModalProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description: string
  credentials: Credential[]
  connectionCommand?: string
}

export function CredentialsModal({
  open,
  onOpenChange,
  title,
  description,
  credentials,
  connectionCommand,
}: CredentialsModalProps) {
  const [confirmedSaved, setConfirmedSaved] = useState(false)
  const [visibleFields, setVisibleFields] = useState<Set<string>>(new Set())
  const { toast } = useToast()

  const handleCopy = async (value: string, label: string) => {
    try {
      await navigator.clipboard.writeText(value)
      toast({
        title: "Copied",
        description: `${label} copied to clipboard`,
      })
    } catch (error) {
      toast({
        title: "Copy failed",
        description: "Failed to copy to clipboard",
        variant: "destructive",
      })
    }
  }

  const toggleVisibility = (label: string) => {
    const newVisible = new Set(visibleFields)
    if (newVisible.has(label)) {
      newVisible.delete(label)
    } else {
      newVisible.add(label)
    }
    setVisibleFields(newVisible)
  }

  const handleClose = () => {
    if (!confirmedSaved) {
      const confirmed = window.confirm(
        "Have you saved these credentials? They won't be shown again."
      )
      if (!confirmed) {
        return
      }
    }
    setConfirmedSaved(false)
    setVisibleFields(new Set())
    onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={(newOpen) => !newOpen && handleClose()}>
      <DialogContent
        className="sm:max-w-md"
        onPointerDownOutside={(e) => e.preventDefault()}
        onEscapeKeyDown={(e) => e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>

        {/* Warning Banner */}
        <div className="bg-yellow-50 border border-yellow-200 rounded p-3">
          <p className="text-sm text-yellow-800">
            ⚠️ Save these credentials now - they won't be shown again!
          </p>
        </div>

        {/* Credentials List */}
        <div className="space-y-3">
          {credentials.map((cred) => (
            <div key={cred.label} className="space-y-1">
              <label className="text-sm font-medium">{cred.label}</label>
              <div className="flex gap-2">
                <div className="flex-1 font-mono text-sm bg-muted p-2 rounded break-all">
                  {cred.sensitive && !visibleFields.has(cred.label)
                    ? "••••••••••••"
                    : cred.value}
                </div>
                {cred.sensitive && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => toggleVisibility(cred.label)}
                    className="shrink-0"
                  >
                    {visibleFields.has(cred.label) ? (
                      <EyeOff className="h-4 w-4" />
                    ) : (
                      <Eye className="h-4 w-4" />
                    )}
                  </Button>
                )}
                {(cred.copyable !== false) && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => handleCopy(cred.value, cred.label)}
                    className="shrink-0"
                  >
                    <Copy className="h-4 w-4" />
                  </Button>
                )}
              </div>
            </div>
          ))}
        </div>

        {/* Connection Command */}
        {connectionCommand && (
          <div className="space-y-1">
            <label className="text-sm font-medium">Connection Command</label>
            <div className="flex gap-2">
              <code className="flex-1 text-sm bg-muted p-2 rounded break-all">
                {connectionCommand}
              </code>
              <Button
                variant="outline"
                size="sm"
                onClick={() => handleCopy(connectionCommand, "Command")}
                className="shrink-0"
              >
                <Copy className="h-4 w-4" />
              </Button>
            </div>
          </div>
        )}

        {/* Confirmation Checkbox */}
        <div className="flex items-center gap-2">
          <input
            type="checkbox"
            id="confirm-saved"
            checked={confirmedSaved}
            onChange={(e) => setConfirmedSaved(e.target.checked)}
            className="h-4 w-4"
          />
          <label htmlFor="confirm-saved" className="text-sm cursor-pointer">
            I have saved these credentials
          </label>
        </div>

        <DialogFooter>
          <Button onClick={handleClose} disabled={!confirmedSaved}>
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
