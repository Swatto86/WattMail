/** Rolling N-day cutoff helpers for the mail list date-range control. */

export type RangeDays = 7 | 14 | 30 | 90 | 0;

export const RANGE_CHOICES: readonly RangeDays[] = [7, 14, 30, 90, 0];

/** Epoch-ms cutoff for a rolling N-day window, or null when showing all mail. */
export function rangeCutoffMs(days: number, nowMs = Date.now()): number | null {
  if (days <= 0) return null;
  return nowMs - days * 86_400_000;
}

/** ISO-8601 cutoff for Graph `$filter=receivedDateTime ge …`, or null for All. */
export function rangeSinceIso(days: number, nowMs = Date.now()): string | null {
  const cutoff = rangeCutoffMs(days, nowMs);
  return cutoff === null ? null : new Date(cutoff).toISOString();
}

export function parseRangeDays(raw: string | null): RangeDays {
  const v = Number.parseInt(raw ?? "", 10);
  return (RANGE_CHOICES as readonly number[]).includes(v) ? (v as RangeDays) : 7;
}

/** Keep messages whose `received` ISO timestamp is on/after the cutoff. */
export function filterByReceivedRange<T extends { received: string }>(
  messages: T[],
  days: number,
  nowMs = Date.now(),
): T[] {
  const cutoff = rangeCutoffMs(days, nowMs);
  if (cutoff === null) return messages;
  return messages.filter((m) => {
    const t = Date.parse(m.received);
    return !Number.isNaN(t) && t >= cutoff;
  });
}

/**
 * Newest-first window already reaches before the cutoff (or range is All /
 * folder exhausted / empty). Used only for folder-scoped coverage; mailbox-wide
 * range mode pages from the server instead.
 */
export function rangeIsCovered(
  messages: { received: string }[],
  days: number,
  reachedOldest: boolean,
  nowMs = Date.now(),
): boolean {
  const cutoff = rangeCutoffMs(days, nowMs);
  if (cutoff === null || reachedOldest) return true;
  if (messages.length === 0) return true;
  const oldest = messages[messages.length - 1];
  const t = Date.parse(oldest.received);
  return !Number.isNaN(t) && t < cutoff;
}
