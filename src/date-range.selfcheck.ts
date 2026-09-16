/**
 * Self-check for date-range helpers.
 * Run: node --experimental-strip-types src/date-range.selfcheck.ts
 */
import {
  filterByReceivedRange,
  parseRangeDays,
  rangeCutoffMs,
  rangeIsCovered,
  rangeSinceIso,
} from "./date-range.ts";

const now = Date.parse("2026-09-16T12:00:00Z");
const day = 86_400_000;

function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(msg);
}

assert(rangeCutoffMs(0, now) === null, "all-mail has no cutoff");
assert(rangeCutoffMs(7, now) === now - 7 * day, "7-day cutoff");
assert(rangeSinceIso(7, now) === new Date(now - 7 * day).toISOString(), "ISO cutoff");
assert(rangeSinceIso(0, now) === null, "all-mail has no ISO cutoff");
assert(parseRangeDays(null) === 7, "default is 7 days");
assert(parseRangeDays("30") === 30, "parses 30");
assert(parseRangeDays("3") === 7, "unknown falls back to 7");

const msgs = [
  { received: "2026-09-15T10:00:00Z" }, // 1d ago
  { received: "2026-09-10T10:00:00Z" }, // 6d ago
  { received: "2026-09-01T10:00:00Z" }, // 15d ago
];

assert(filterByReceivedRange(msgs, 7, now).length === 2, "keeps last 7 days");
assert(filterByReceivedRange(msgs, 0, now).length === 3, "all keeps everything");
assert(rangeIsCovered(msgs, 7, false, now) === true, "oldest past cutoff => covered");
assert(
  rangeIsCovered(msgs.slice(0, 2), 7, false, now) === false,
  "window still inside range => not covered",
);
assert(rangeIsCovered(msgs.slice(0, 2), 7, true, now) === true, "exhausted => covered");

console.log("date-range self-check ok");
