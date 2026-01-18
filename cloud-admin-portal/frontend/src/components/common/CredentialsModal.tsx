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
import { Eye, EyeOff, Copy, Check, ShieldCheck, Terminal } from "lucide-react"

export interface Credential {
  label: string
  value: string
  sensitive?: boolean
  copyable?: boolean
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
  const [copiedField, setCopiedField] = useState<string | null>(null)
  const { toast } = useToast()

  const handleCopy = async (value: string, label: string) => {
    try {
      await navigator.clipboard.writeText(value)
      setCopiedField(label)
      setTimeout(() => setCopiedField(null), 2000)
    } catch {
      const textArea = document.createElement('textarea')
      textArea.value = value
      textArea.style.position = 'fixed'
      textArea.style.left = '-9999px'
      document.body.appendChild(textArea)
      textArea.select()
      try {
        document.execCommand('copy')
        setCopiedField(label)
        setTimeout(() => setCopiedField(null), 2000)
      } catch {
        toast({
          title: "Copy failed",
          description: "Please copy manually",
          variant: "destructive",
        })
      } finally {
        document.body.removeChild(textArea)
      }
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
        className="sm:max-w-[480px]"
        onPointerDownOutside={(e) => e.preventDefault()}
        onEscapeKeyDown={(e) => e.preventDefault()}
      >
        <DialogHeader className="pb-2">
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 rounded-full bg-green-500/10 flex items-center justify-center">
              <ShieldCheck className="w-5 h-5 text-green-600" />
            </div>
            <div>
              <DialogTitle className="text-lg">{title}</DialogTitle>
              <DialogDescription className="text-xs mt-0.5">{description}</DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div className="space-y-4 py-2">
          {credentials.map((cred) => (
            <div key={cred.label} className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">{cred.label}</label>
              <div className="flex gap-2">
                <div className="flex-1 font-mono text-sm bg-muted/50 border border-border/50 px-3 py-2 rounded-md break-all">
                  {cred.sensitive && !visibleFields.has(cred.label)
                    ? "••••••••••••••••"
                    : cred.value}
                </div>
                <div className="flex gap-1">
                  {cred.sensitive && (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => toggleVisibility(cred.label)}
                      className="h-9 w-9 p-0 text-muted-foreground hover:text-foreground"
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
                      variant="ghost"
                      size="sm"
                      onClick={() => handleCopy(cred.value, cred.label)}
                      className="h-9 w-9 p-0 text-muted-foreground hover:text-foreground"
                    >
                      {copiedField === cred.label ? (
                        <Check className="h-4 w-4 text-green-500" />
                      ) : (
                        <Copy className="h-4 w-4" />
                      )}
                    </Button>
                  )}
                </div>
              </div>
            </div>
          ))}

          {connectionCommand && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
                <Terminal className="w-3 h-3" />
                Connection Command
              </label>
              <div className="flex gap-2">
                <code className="flex-1 text-xs bg-muted/50 border border-border/50 px-3 py-2.5 rounded-md break-all font-mono text-muted-foreground">
                  {connectionCommand}
                </code>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => handleCopy(connectionCommand, "Command")}
                  className="h-9 w-9 p-0 text-muted-foreground hover:text-foreground shrink-0"
                >
                  {copiedField === "Command" ? (
                    <Check className="h-4 w-4 text-green-500" />
                  ) : (
                    <Copy className="h-4 w-4" />
                  )}
                </Button>
              </div>
            </div>
          )}
        </div>

        <div className="border-t pt-4 mt-2">
          <label className="flex items-start gap-3 cursor-pointer group">
            <input
              type="checkbox"
              checked={confirmedSaved}
              onChange={(e) => setConfirmedSaved(e.target.checked)}
              className="mt-0.5 h-4 w-4 rounded border-border"
            />
            <div className="space-y-0.5">
              <span className="text-sm font-medium group-hover:text-foreground transition-colors">
                I have saved these credentials
              </span>
              <p className="text-[11px] text-muted-foreground leading-relaxed">
                These credentials are shown only once and cannot be recovered
              </p>
            </div>
          </label>
        </div>

        <DialogFooter className="mt-2">
          <Button 
            onClick={handleClose} 
            disabled={!confirmedSaved}
            className="w-full sm:w-auto"
          >
            Done
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
