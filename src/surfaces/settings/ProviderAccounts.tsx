import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import {
  accountsFor,
  createAccount,
  DEFAULT_ACCOUNT_ID,
  getAccountsSnapshot,
  loadDefaultAccountId,
  refreshAccounts,
  removeAccount,
  renameAccount,
  saveDefaultAccountId,
  subscribeAccounts,
  type Account,
  type AccountProvider,
} from "../../lib/accounts";
import {
  fetchClaudeRateLimits,
  fetchCodexRateLimits,
} from "../../lib/rateLimitsFetch";
import {
  formatRateLimitWindowChipLabel,
  formatUsagePercent,
  type ProviderRateLimits,
  type RateLimitWindow,
} from "../../lib/rateLimits";
import { SecondaryButton } from "./controls";
import { AccountLoginTerminal } from "./AccountLoginTerminal";

function fetchUsage(
  provider: AccountProvider,
  account: Account,
): Promise<ProviderRateLimits> {
  return provider === "claude"
    ? fetchClaudeRateLimits(account.profileDir)
    : fetchCodexRateLimits(account);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function ProviderAccounts({ provider }: { provider: AccountProvider }) {
  useSyncExternalStore(
    subscribeAccounts,
    getAccountsSnapshot,
    getAccountsSnapshot,
  );
  const accounts = accountsFor(provider);
  const accountKey = accounts.map((account) => account.id).join(",");
  const [usage, setUsage] = useState<Record<string, ProviderRateLimits>>({});
  const [pendingLogin, setPendingLogin] = useState<Account | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const defaultId = loadDefaultAccountId(provider);

  useEffect(() => {
    void refreshAccounts();
  }, []);

  const refreshUsage = useCallback(() => {
    for (const account of accountsFor(provider)) {
      void fetchUsage(provider, account).then((limits) => {
        setUsage((prev) => ({ ...prev, [account.id]: limits }));
        setNow(Date.now());
      });
    }
  }, [provider]);

  useEffect(() => {
    void accountKey;
    refreshUsage();
  }, [accountKey, refreshUsage]);

  const onAdd = async () => {
    const label = window.prompt("Name this account (for example Work):");
    if (!label?.trim()) return;
    setError(null);
    try {
      setPendingLogin(await createAccount(provider, label.trim()));
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onRename = async (account: Account) => {
    const label = window.prompt("Rename account:", account.label);
    if (!label?.trim() || label.trim() === account.label) return;
    setError(null);
    try {
      await renameAccount(account.id, label.trim());
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onRemove = async (account: Account) => {
    const ok = window.confirm(
      `Remove ${account.label}? Its saved login will be deleted.`,
    );
    if (!ok) return;
    setError(null);
    try {
      await removeAccount(account.id);
      if (defaultId === account.id) {
        saveDefaultAccountId(provider, DEFAULT_ACCOUNT_ID);
      }
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onLoginDone = useCallback(() => {
    setPendingLogin(null);
    void refreshAccounts().then(refreshUsage);
  }, [refreshUsage]);

  return (
    <div className="mb-4 rounded-md border border-content/10">
      {accounts.map((account) => (
        <AccountRow
          key={account.id}
          account={account}
          isDefault={defaultId === account.id}
          limits={usage[account.id]}
          now={now}
          onDefault={() => saveDefaultAccountId(provider, account.id)}
          onRename={() => void onRename(account)}
          onRemove={() => void onRemove(account)}
          onSignIn={() => setPendingLogin(account)}
        />
      ))}
      <div className="flex items-center justify-between gap-3 px-3 py-2">
        <span className="text-[12px] text-content/45">
          Each account signs in once and keeps its own login. Config and
          history are shared.
        </span>
        <SecondaryButton onClick={() => void onAdd()}>
          Add account
        </SecondaryButton>
      </div>
      {error ? (
        <p className="px-3 pb-2 text-[12px] text-red-400">{error}</p>
      ) : null}
      {pendingLogin ? (
        <AccountLoginTerminal
          account={pendingLogin}
          onDone={onLoginDone}
          onCancel={() => setPendingLogin(null)}
        />
      ) : null}
    </div>
  );
}

interface AccountRowProps {
  account: Account;
  isDefault: boolean;
  limits?: ProviderRateLimits;
  now: number;
  onDefault: () => void;
  onRename: () => void;
  onRemove: () => void;
  onSignIn: () => void;
}

function AccountRow({
  account,
  isDefault,
  limits,
  now,
  onDefault,
  onRename,
  onRemove,
  onSignIn,
}: AccountRowProps) {
  const isSignedOut = limits?.status === "unavailable";
  const isCustom = account.id !== DEFAULT_ACCOUNT_ID;
  return (
    <div className="flex items-center gap-3 border-b border-content/5 px-3 py-2 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-[13px] text-content">
          {account.label}
          {isDefault ? (
            <span className="rounded-full bg-content/10 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-content/60">
              Default
            </span>
          ) : null}
        </div>
        <div className="truncate text-[12px] text-content/45">
          {account.identity ?? (isSignedOut ? "Not signed in" : "Signed in")}
        </div>
      </div>
      <UsageSummary limits={limits} now={now} />
      {isSignedOut && isCustom ? (
        <SecondaryButton onClick={onSignIn}>Sign in</SecondaryButton>
      ) : null}
      <SecondaryButton onClick={onDefault} disabled={isDefault}>
        {isDefault ? "Default" : "Use by default"}
      </SecondaryButton>
      {isCustom ? (
        <>
          <SecondaryButton onClick={onRename}>Rename</SecondaryButton>
          <SecondaryButton onClick={onRemove} danger>
            Remove
          </SecondaryButton>
        </>
      ) : null}
    </div>
  );
}

function UsageSummary({
  limits,
  now,
}: {
  limits?: ProviderRateLimits;
  now: number;
}) {
  if (!limits || limits.status === "idle" || limits.status === "fetching") {
    return <span className="text-[12px] text-content/35">···</span>;
  }
  if (limits.status === "unavailable") {
    return <span className="text-[12px] text-content/35">not connected</span>;
  }
  if (limits.status === "error") {
    return (
      <span className="text-[12px] text-content/35" title={limits.error ?? ""}>
        usage unavailable
      </span>
    );
  }
  const windows = [limits.session, limits.weekly].filter(
    (window): window is RateLimitWindow => window != null,
  );
  if (windows.length === 0) {
    return <span className="text-[12px] text-content/35">no usage data</span>;
  }
  return (
    <span className="flex items-center gap-2 text-[12px] tabular-nums text-content/70">
      {windows.map((window) => (
        <span key={window.windowMinutes}>
          {formatUsagePercent(window.usedPercent)}{" "}
          {formatRateLimitWindowChipLabel(window, now)}
        </span>
      ))}
    </span>
  );
}
