import { useState, useMemo } from "react"

export type SortDirection = "asc" | "desc" | null

export interface SortConfig<K extends string> {
  key: K | null
  direction: SortDirection
}

export type Comparator<T> = (a: T, b: T) => number

export interface SortableColumnConfig<T, K extends string> {
  key: K
  comparator?: Comparator<T>
  getValue?: (item: T) => unknown
}

function defaultComparator<T>(getValue: (item: T) => unknown) {
  return (a: T, b: T): number => {
    const valA = getValue(a)
    const valB = getValue(b)

    // Handle null/undefined
    if (valA == null && valB == null) return 0
    if (valA == null) return 1
    if (valB == null) return -1

    // Handle booleans
    if (typeof valA === "boolean" && typeof valB === "boolean") {
      return valA === valB ? 0 : valA ? -1 : 1
    }

    // Handle numbers
    if (typeof valA === "number" && typeof valB === "number") {
      return valA - valB
    }

    // Handle dates
    if (valA instanceof Date && valB instanceof Date) {
      return valA.getTime() - valB.getTime()
    }

    // Handle arrays (compare by length)
    if (Array.isArray(valA) && Array.isArray(valB)) {
      return valA.length - valB.length
    }

    // Default to string comparison
    return String(valA).localeCompare(String(valB))
  }
}

export function useSortableData<T, K extends string>(
  data: T[] | undefined,
  columns: SortableColumnConfig<T, K>[],
  defaultSort?: SortConfig<K>
) {
  const [sortConfig, setSortConfig] = useState<SortConfig<K>>(
    defaultSort ?? { key: null, direction: null }
  )

  const sortedData = useMemo(() => {
    if (!data || !sortConfig.key || !sortConfig.direction) {
      return data ?? []
    }

    const column = columns.find((c) => c.key === sortConfig.key)
    if (!column) return data

    const comparator =
      column.comparator ??
      defaultComparator<T>(column.getValue ?? ((item) => (item as Record<string, unknown>)[sortConfig.key!]))

    const sorted = [...data].sort((a, b) => {
      const result = comparator(a, b)
      return sortConfig.direction === "desc" ? -result : result
    })

    return sorted
  }, [data, sortConfig, columns])

  const requestSort = (key: K) => {
    setSortConfig((prev) => {
      if (prev.key !== key) {
        return { key, direction: "asc" }
      }
      if (prev.direction === "asc") {
        return { key, direction: "desc" }
      }
      if (prev.direction === "desc") {
        return { key: null, direction: null }
      }
      return { key, direction: "asc" }
    })
  }

  const getSortDirection = (key: K): SortDirection => {
    return sortConfig.key === key ? sortConfig.direction : null
  }

  return {
    sortedData,
    sortConfig,
    requestSort,
    getSortDirection,
  }
}
