/**
 * Create user dialog
 */

import { useState } from "react"
import { useCreateUser } from "@/api/users"
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

interface CreateUserDialogProps {
  tenantName: string
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CreateUserDialog({ tenantName, open, onOpenChange }: CreateUserDialogProps) {
  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [isSuperuser, setIsSuperuser] = useState(false)
  const mutation = useCreateUser(tenantName)
  const { toast } = useToast()

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()

    try {
      const result = await mutation.mutateAsync({
        username: username,
        password: password || undefined,
        superuser: isSuperuser,
      })

      toast({
        title: "User Created",
        description: (
          <div className="mt-2 space-y-2">
            <p>Username: <strong>{result.username}</strong></p>
            <p>Password: <code className="bg-muted px-1 rounded">{result.password}</code></p>
            <p className="text-xs text-muted-foreground mt-2">
              Save this password - it won't be shown again.
            </p>
          </div>
        ),
      })

      // Reset form
      setUsername("")
      setPassword("")
      setIsSuperuser(false)
      onOpenChange(false)
    } catch (error) {
      const message = error instanceof ApiError ? error.message : "Failed to create user"
      toast({
        title: "Error",
        description: message,
        variant: "destructive",
      })
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create User</DialogTitle>
          <DialogDescription>
            Create a new database user for tenant "{tenantName}".
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="username">Username</Label>
            <Input
              id="username"
              value={username}
              onChange={(e) => setUsername(e.target.value.toLowerCase())}
              placeholder="appuser"
              pattern="[a-z0-9_]+"
              minLength={1}
              maxLength={64}
              required
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="password">Password</Label>
            <Input
              id="password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="Auto-generate if empty"
            />
          </div>

          <div className="flex items-center space-x-2">
            <input
              type="checkbox"
              id="is_superuser"
              checked={isSuperuser}
              onChange={(e) => setIsSuperuser(e.target.checked)}
              className="h-4 w-4 rounded border-gray-300"
            />
            <Label htmlFor="is_superuser" className="text-sm font-normal">
              Superuser (full database privileges)
            </Label>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending ? "Creating..." : "Create User"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
