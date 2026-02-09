import { type ClassValue, clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Parse an ISO 8601 timestamp string from the backend.
 * Handles nanosecond precision (truncates to ms) and timezone offsets like +00:00.
 * Example input:  "2026-02-09T05:39:15.897780611+00:00"
 * Example output: Date("2026-02-09T05:39:15.897+00:00")
 */
export function parseDate(iso: string | null | undefined): Date | null {
  if (!iso) return null
  // Truncate sub-second digits beyond 3 (JS Date only supports milliseconds)
  const truncated = iso.replace(/(\.\d{3})\d*/, "$1")
  const d = new Date(truncated)
  return isNaN(d.getTime()) ? null : d
}

/**
 * Format a backend ISO timestamp to a localized date string, or "-" if invalid.
 */
export function formatDate(iso: string | null | undefined): string {
  const d = parseDate(iso)
  return d ? d.toLocaleString() : "-"
}
