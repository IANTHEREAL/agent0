import { ChevronUp, ChevronDown, ChevronsUpDown } from "lucide-react"
import { cn } from "@/lib/utils"
import type { SortDirection } from "@/hooks/useSortableData"

interface SortableHeaderProps {
  children: React.ReactNode
  direction: SortDirection
  onSort: () => void
  className?: string
  align?: "left" | "center" | "right"
}

export function SortableHeader({
  children,
  direction,
  onSort,
  className,
  align = "left",
}: SortableHeaderProps) {
  const alignClass = {
    left: "text-left justify-start",
    center: "text-center justify-center",
    right: "text-right justify-end",
  }[align]

  return (
    <th
      className={cn(
        "px-3 py-2 text-xs font-medium text-muted-foreground cursor-pointer select-none hover:bg-muted/70 transition-colors",
        className
      )}
      onClick={onSort}
    >
      <div className={cn("flex items-center gap-1", alignClass)}>
        <span>{children}</span>
        <span className="w-4 h-4 flex items-center justify-center">
          {direction === "asc" && (
            <ChevronUp className="w-3.5 h-3.5 text-foreground" />
          )}
          {direction === "desc" && (
            <ChevronDown className="w-3.5 h-3.5 text-foreground" />
          )}
          {direction === null && (
            <ChevronsUpDown className="w-3.5 h-3.5 opacity-40" />
          )}
        </span>
      </div>
    </th>
  )
}
