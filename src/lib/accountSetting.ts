import {
  ACCOUNT_SETTING_ID,
  type Account,
  type AccountProvider,
} from "./accounts";
import type { ModelSetting } from "./models";

export function accountOptionLabel(account: Account): string {
  if (account.kind === "apiKey") return `${account.label} · API key`;
  return account.identity
    ? `${account.label} · ${account.identity}`
    : account.label;
}

/**
 * A synthetic model setting that lets the picker choose the session's
 * account. Null when the provider has fewer than two accounts, so the row
 * only appears once there is something to choose.
 */
export function accountModelSetting(
  provider: AccountProvider,
  accounts: Account[],
  current: string,
): ModelSetting | null {
  const mine = accounts.filter(
    (account) => account.provider === provider && account.enabled !== false,
  );
  if (mine.length < 2) return null;
  return {
    id: ACCOUNT_SETTING_ID,
    label: "Account",
    kind: "select",
    value: current,
    description: "Which signed-in account this session runs on.",
    options: mine.map((account) => ({
      value: account.id,
      label: accountOptionLabel(account),
    })),
  };
}
