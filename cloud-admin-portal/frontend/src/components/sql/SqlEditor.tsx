import { useState, useRef, useEffect } from "react"
import { Play, Loader2, RotateCcw, Terminal, CheckCircle2, XCircle, Clock } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { useExecuteQuery } from "@/api/tenants"

interface SqlEditorProps {
  tenantId: string
}

export function SqlEditor({ tenantId }: SqlEditorProps) {
  const [sql, setSql] = useState("")
  const [result, setResult] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [executionTime, setExecutionTime] = useState<number | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const mutation = useExecuteQuery(tenantId)

  useEffect(() => {
    if (textareaRef.current) {
      textareaRef.current.style.height = "auto"
      textareaRef.current.style.height = `${Math.max(140, textareaRef.current.scrollHeight)}px`
    }
  }, [sql])

  const handleExecute = async () => {
    if (!sql.trim()) return
    
    setResult(null)
    setError(null)
    setExecutionTime(null)
    
    const startTime = performance.now()
    
    try {
      const response = await mutation.mutateAsync(sql)
      const endTime = performance.now()
      setExecutionTime(Math.round(endTime - startTime))
      
      if (response.success) {
        setResult(response.result || "(Query executed successfully, no output)")
      } else {
        setError(response.error || "Query failed")
      }
    } catch (e) {
      const endTime = performance.now()
      setExecutionTime(Math.round(endTime - startTime))
      setError(e instanceof Error ? e.message : "Query failed")
    }
  }

  const handleClear = () => {
    setSql("")
    setResult(null)
    setError(null)
    setExecutionTime(null)
    textareaRef.current?.focus()
  }

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault()
      handleExecute()
    }
  }

  const hasOutput = result !== null || error !== null

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Terminal className="h-4 w-4 text-muted-foreground" />
            <CardTitle className="text-base">SQL Editor</CardTitle>
          </div>
          {hasOutput && (
            <Button
              variant="ghost"
              size="sm"
              onClick={handleClear}
              className="h-7 text-xs text-muted-foreground hover:text-foreground"
            >
              <RotateCcw className="w-3 h-3 mr-1" />
              Clear
            </Button>
          )}
        </div>
        <CardDescription className="text-xs">
          Execute SQL queries directly on this tenant's database
        </CardDescription>
      </CardHeader>
      
      <CardContent className="space-y-4">
        <div className="relative group">
          <div className="absolute left-0 top-0 bottom-0 w-10 bg-muted/50 rounded-l-lg border-r flex flex-col items-center pt-3 text-[10px] text-muted-foreground font-mono select-none">
            {sql.split('\n').map((_, i) => (
              <div key={i} className="h-[20px] leading-[20px]">{i + 1}</div>
            ))}
          </div>
          <textarea
            ref={textareaRef}
            value={sql}
            onChange={(e) => setSql(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="-- Enter your SQL query here&#10;SELECT * FROM pg_catalog.pg_roles LIMIT 10;"
            className="w-full min-h-[140px] pl-12 pr-3 py-3 font-mono text-sm leading-[20px] bg-muted/30 rounded-lg border border-border/50 resize-none focus:outline-none focus:ring-2 focus:ring-ring focus:border-transparent transition-all placeholder:text-muted-foreground/50"
            spellCheck={false}
          />
        </div>
        
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <kbd className="hidden sm:inline-flex items-center gap-1 px-2 py-1 text-[10px] font-mono bg-muted rounded border text-muted-foreground">
              <span className="text-[9px]">⌘</span>Enter
            </kbd>
            <span className="text-xs text-muted-foreground">to execute</span>
          </div>
          <Button
            size="sm"
            onClick={handleExecute}
            disabled={mutation.isPending || !sql.trim()}
            className="h-8 px-4 gap-2"
          >
            {mutation.isPending ? (
              <>
                <Loader2 className="w-3.5 h-3.5 animate-spin" />
                Running...
              </>
            ) : (
              <>
                <Play className="w-3.5 h-3.5" />
                Execute
              </>
            )}
          </Button>
        </div>

        {hasOutput && (
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              {error ? (
                <XCircle className="w-4 h-4 text-destructive" />
              ) : (
                <CheckCircle2 className="w-4 h-4 text-green-500" />
              )}
              <span className={`text-sm font-medium ${error ? "text-destructive" : "text-green-600"}`}>
                {error ? "Error" : "Success"}
              </span>
              {executionTime !== null && (
                <span className="flex items-center gap-1 text-xs text-muted-foreground ml-auto">
                  <Clock className="w-3 h-3" />
                  {executionTime}ms
                </span>
              )}
            </div>
            
            <div className={`rounded-lg border overflow-hidden ${
              error 
                ? "bg-destructive/5 border-destructive/20" 
                : "bg-muted/30 border-border/50"
            }`}>
              <div className={`px-3 py-1.5 text-[10px] font-medium uppercase tracking-wider border-b ${
                error 
                  ? "bg-destructive/10 text-destructive border-destructive/20" 
                  : "bg-muted/50 text-muted-foreground border-border/50"
              }`}>
                {error ? "Error Details" : "Query Result"}
              </div>
              <div className="p-3 max-h-[300px] overflow-auto">
                <pre className={`text-sm font-mono whitespace-pre-wrap break-words ${
                  error ? "text-destructive" : "text-foreground"
                }`}>
                  {error || result}
                </pre>
              </div>
            </div>
          </div>
        )}

        {!hasOutput && !mutation.isPending && (
          <div className="flex items-center justify-center py-6 text-muted-foreground border border-dashed rounded-lg bg-muted/20">
            <div className="text-center">
              <Terminal className="w-8 h-8 mx-auto mb-2 opacity-50" />
              <p className="text-sm">Query results will appear here</p>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}
