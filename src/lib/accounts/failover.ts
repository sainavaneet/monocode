import {
  accountIdFor,
  accountsFor,
  findAccount,
  isAccountProvider,
  loadAutoSwitchAccount,
  refreshAccounts,
  withAccountSetting,
  type Account,
  type AccountProvider,
} from "../accounts";
import { accountOptionLabel } from "../accountSetting";
import type { HarnessEvent, SendTurnInput } from "../harness/types";
import type { HarnessId } from "../session";
import type { ProviderRateLimits } from "../rateLimits";
import {
  fetchClaudeRateLimits,
  fetchCodexRateLimits,
} from "../rateLimitsFetch";
import { isRateLimitFailure } from "./rateLimitDetect";
import { selectFailover } from "./selectFailover";

function fetchUsage(
  provider: AccountProvider,
  account: Account,
): Promise<ProviderRateLimits> {
  return provider === "claude"
    ? fetchClaudeRateLimits(account.profileDir)
    : fetchCodexRateLimits(account);
}

/**
 * Refreshes usage for every account of `provider` and picks the one a
 * rate-limited session should continue on. Null when there is no other
 * account with quota.
 */
export async function chooseFailoverAccount(
  provider: AccountProvider,
  currentId: string,
): Promise<Account | null> {
  await refreshAccounts();
  const accounts = accountsFor(provider);
  if (accounts.length < 2 || !findAccount(provider, currentId)) return null;
  const entries = await Promise.all(
    accounts.map(async (account) => {
      const limits = await fetchUsage(provider, account).catch(
        (): ProviderRateLimits | undefined => undefined,
      );
      return [account.id, limits] as const;
    }),
  );
  const usage: Record<string, ProviderRateLimits> = Object.fromEntries(
    entries.filter(
      (entry): entry is readonly [string, ProviderRateLimits] =>
        entry[1] != null,
    ),
  );
  return selectFailover({ provider, accounts, usage, exclude: currentId });
}

export function failoverStatusText(account: Account): string {
  return `Switched to ${accountOptionLabel(account)} after a rate limit`;
}

function errorMessage(error: unknown): string | null {
  if (error == null) return null;
  return error instanceof Error ? error.message : String(error);
}

type TurnInput = SendTurnInput & { harness: HarnessId };
type HeldError = Extract<HarnessEvent, { type: "session.error" }>;
type SendTurn = (input: TurnInput) => Promise<void>;

/**
 * Runs a turn and, when it fails on a rate limit, retries it once on the
 * same provider's account with the most quota left.
 *
 * The rate-limit `session.error` is held back while the retry is decided:
 * if another account takes over, the transcript shows a status line instead
 * of an error, and a `session.configChanged` event rebinds the session to
 * the new account so later turns and the picker follow it. If no account
 * can take over, the held error is delivered exactly as it would have been.
 */
export async function sendTurnWithFailover(
  input: TurnInput,
  send: SendTurn,
  options: { enabled?: boolean } = {},
): Promise<void> {
  const enabled = options.enabled ?? loadAutoSwitchAccount();
  if (!enabled || !isAccountProvider(input.harness)) return send(input);
  const provider: AccountProvider = input.harness;

  const held: { current: HeldError | null } = { current: null };
  const onEvent = (event: HarnessEvent) => {
    if (
      event.type === "session.error" &&
      held.current == null &&
      isRateLimitFailure(event.message)
    ) {
      held.current = event;
      return;
    }
    input.onEvent(event);
  };

  let thrown: unknown = null;
  try {
    await send({ ...input, onEvent });
  } catch (error: unknown) {
    thrown = error;
  }

  const release = () => {
    if (held.current) input.onEvent(held.current);
    if (thrown) throw thrown;
  };

  const failure = held.current?.message ?? errorMessage(thrown);
  if (!isRateLimitFailure(failure)) return release();

  const fromId = accountIdFor(provider, input.modelSettings);
  const next = await chooseFailoverAccount(provider, fromId).catch(
    (): Account | null => null,
  );
  if (!next) return release();

  const modelSettings = withAccountSetting(input.modelSettings ?? {}, next.id);
  input.onEvent({ type: "status", text: failoverStatusText(next) });
  input.onEvent({ type: "session.configChanged", modelSettings });
  await send({ ...input, modelSettings });
}
