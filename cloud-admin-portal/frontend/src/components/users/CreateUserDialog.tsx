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
import { CredentialsModal } from "@/components/common/CredentialsModal"
import type { UserCreateResponse } from "@/types"

interface CreateUserDialogProps {
  tenantId: string
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CreateUserDialog({ tenantId, open, onOpenChange }: CreateUserDialogProps) {
  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [isSuperuser, setIsSuperuser] = useState(false)
  const [showCredentials, setShowCredentials] = useState(false)
  const [createdUser, setCreatedUser] = useState<UserCreateResponse | null>(null)

  const mutation = useCreateUser(tenantId)
  const { toast } = useToast()

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()

    try {
      const result = await mutation.mutateAsync({
        username: username,
        password: password || undefined,
        superuser: isSuperuser,
      })

      // Save result and show credentials modal
      setCreatedUser(result)
      setShowCredentials(true)

      // Close creation dialog
      onOpenChange(false)

      // Show simple success toast
      toast({
        title: "User Created",
        description: `User "${result.username}" has been created successfully.`,
      })

      // Reset form
      setUsername("")
      setPassword("")
      setIsSuperuser(false)
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
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Create User</DialogTitle>
            <DialogDescription>
              Create a new database user for tenant "{tenantId}".
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

      {/* Credentials Display Modal */}
      {createdUser && (
        <CredentialsModal
          open={showCredentials}
          onOpenChange={setShowCredentials}
          title="User Created Successfully"
          description={`User "${createdUser.username}" has been created. Save these credentials - they won't be shown again.`}
          credentials={[
            {
              label: "Username",
              value: createdUser.username,
              copyable: true,
            },
            {
              label: "Password",
              value: createdUser.password,
              sensitive: true,
              copyable: true,
            },
          ]}
          connectionCommand={createdUser.connection}
        />
      )}
    </>
  )
}
