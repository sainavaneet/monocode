import type { Account, AccountProvider } from "../accounts";
import type { ProviderRateLimits } from "../rateLimits";

const EXHAUSTED_PERCENT = 100;

/** The most-used window for an account; 0 when usage is unknown. */
export function tightestUsedPercent(
  limits: ProviderRateLimits | undefined,
): number {
  if (!limits || limits.status !== "ok") return 0;
  const windows = [limits.session, limits.weekly].filter(
    (window) => window != null,
  );
  return windows.reduce((max, window) => Math.max(max, window.usedPercent), 0);
}

/**
 * Picks the account a rate-limited session should move to: same provider,
 * not the one that just failed, most quota left. Accounts with no usage
 * data count as available. Null when nothing qualifies.
 */
export function selectFailover(input: {
  provider: AccountProvider;
  accounts: Account[];
  usage: Record<string, ProviderRateLimits>;
  exclude: string;
}): Account | null {
  const candidates = input.accounts
    .filter(
      (account) =>
        account.provider === input.provider && account.id !== input.exclude,
    )
    .map((account) => ({
      account,
      used: tightestUsedPercent(input.usage[account.id]),
    }))
    .filter((entry) => entry.used < EXHAUSTED_PERCENT)
    .sort((a, b) => a.used - b.used);
  return candidates[0]?.account ?? null;
}
