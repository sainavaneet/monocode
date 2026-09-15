/**
 * Messages Claude Code and Codex produce when a login has no quota left.
 * Kept loose on purpose: both CLIs reword these between releases.
 */
const RATE_LIMIT_PATTERNS: RegExp[] = [
  /hit your limit/i,
  /usage[_ -]?limit/i,
  /rate[_ -]?limit/i,
  /\b429\b/,
  /too many requests/i,
  /quota/i,
  /out of (extra )?usage/i,
];

export function isRateLimitFailure(
  message: string | null | undefined,
): boolean {
  if (!message) return false;
  return RATE_LIMIT_PATTERNS.some((pattern) => pattern.test(message));
}
