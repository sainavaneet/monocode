import {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import {
  accountsFor,
  createAccount,
  createApiKeyAccount,
  DEFAULT_ACCOUNT_ID,
  getAccountsSnapshot,
  isAccountEnabled,
  isApiKeyAccount,
  loadDefaultAccountId,
  refreshAccounts,
  removeAccount,
  renameAccount,
  saveDefaultAccountId,
  setAccountEnabled,
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
import { SecondaryButton, Toggle } from "./controls";
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
  // Which inline form is open: a new login, a new API key, or a rename.
  const [naming, setNaming] = useState<
    "new" | "newApiKey" | { renameId: string } | null
  >(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const defaultId = loadDefaultAccountId(provider);

  useEffect(() => {
    void refreshAccounts();
  }, []);

  const refreshUsage = useCallback(() => {
    for (const account of accountsFor(provider)) {
      // An API key has no quota window; nothing to fetch.
      if (isApiKeyAccount(account)) continue;
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

  const onCreate = async (label: string) => {
    setNaming(null);
    setError(null);
    try {
      setPendingLogin(await createAccount(provider, label));
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onCreateApiKey = async (label: string, key: string) => {
    setNaming(null);
    setError(null);
    try {
      await createApiKeyAccount(provider, label, key);
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onToggleEnabled = async (account: Account, enabled: boolean) => {
    setError(null);
    try {
      await setAccountEnabled(account, enabled);
    } catch (failure: unknown) {
      setError(errorText(failure));
    }
  };

  const onRename = async (account: Account, label: string) => {
    setNaming(null);
    if (label === account.label) return;
    setError(null);
    try {
      await renameAccount(account, label);
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
      {accounts.map((account) =>
        typeof naming === "object" && naming?.renameId === account.id ? (
          <NameField
            key={account.id}
            label="Rename account"
            initial={account.label}
            submitLabel="Save"
            onSubmit={(label) => void onRename(account, label)}
            onCancel={() => setNaming(null)}
          />
        ) : (
          <AccountRow
            key={account.id}
            account={account}
            isDefault={defaultId === account.id}
            limits={usage[account.id]}
            now={now}
            onDefault={() => saveDefaultAccountId(provider, account.id)}
            onRename={() => setNaming({ renameId: account.id })}
            onRemove={() => void onRemove(account)}
            onSignIn={() => setPendingLogin(account)}
            onToggleEnabled={(enabled) => void onToggleEnabled(account, enabled)}
          />
        ),
      )}
      {naming === "new" ? (
        <NameField
          label="Name this account"
          placeholder="For example Work"
          submitLabel="Create and sign in"
          onSubmit={(label) => void onCreate(label)}
          onCancel={() => setNaming(null)}
        />
      ) : null}
      {naming === "newApiKey" ? (
        <ApiKeyField
          provider={provider}
          onSubmit={(label, key) => void onCreateApiKey(label, key)}
          onCancel={() => setNaming(null)}
        />
      ) : null}
      <div className="flex items-center justify-between gap-3 px-3 py-2">
        <span className="text-[12px] text-content/45">
          Each account signs in once and keeps its own login. Config and
          history are shared. API keys stay off until you enable them.
        </span>
        <div className="flex shrink-0 items-center gap-2">
          <SecondaryButton
            onClick={() => setNaming("newApiKey")}
            disabled={naming != null || pendingLogin != null}
          >
            Add API key
          </SecondaryButton>
          <SecondaryButton
            onClick={() => setNaming("new")}
            disabled={naming != null || pendingLogin != null}
          >
            Add account
          </SecondaryButton>
        </div>
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

interface NameFieldProps {
  label: string;
  initial?: string;
  placeholder?: string;
  submitLabel: string;
  onSubmit: (label: string) => void;
  onCancel: () => void;
}

/**
 * Inline text entry. The Tauri webview does not implement `window.prompt`,
 * so names are collected in the list itself.
 */
function NameField({
  label,
  initial = "",
  placeholder,
  submitLabel,
  onSubmit,
  onCancel,
}: NameFieldProps) {
  const [value, setValue] = useState(initial);
  const input = useRef<HTMLInputElement>(null);
  const trimmed = value.trim();

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);

  const submit = () => {
    if (trimmed) onSubmit(trimmed);
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      submit();
    } else if (event.key === "Escape") {
      event.preventDefault();
      onCancel();
    }
  };

  return (
    <div className="flex items-center gap-2 border-b border-content/5 px-3 py-2 last:border-b-0">
      <input
        ref={input}
        aria-label={label}
        value={value}
        placeholder={placeholder}
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={onKeyDown}
        className="min-w-0 flex-1 rounded-md border border-content/10 bg-content/5 px-2 py-1 text-[13px] text-content outline-none placeholder:text-content/35 focus:border-content/30"
      />
      <SecondaryButton onClick={submit} disabled={!trimmed}>
        {submitLabel}
      </SecondaryButton>
      <SecondaryButton onClick={onCancel}>Cancel</SecondaryButton>
    </div>
  );
}

const API_KEY_PLACEHOLDER: Record<AccountProvider, string> = {
  claude: "sk-ant-…",
  codex: "sk-…",
};

interface ApiKeyFieldProps {
  provider: AccountProvider;
  onSubmit: (label: string, key: string) => void;
  onCancel: () => void;
}

/** Name plus masked key. The key goes straight to the backend and is never
 * shown again; only its last four characters come back as a hint. */
function ApiKeyField({ provider, onSubmit, onCancel }: ApiKeyFieldProps) {
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const labelInput = useRef<HTMLInputElement>(null);
  const canSubmit = label.trim().length > 0 && key.trim().length > 0;

  useEffect(() => {
    labelInput.current?.focus();
  }, []);

  const submit = () => {
    if (canSubmit) onSubmit(label.trim(), key.trim());
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      submit();
    } else if (event.key === "Escape") {
      event.preventDefault();
      onCancel();
    }
  };

  const inputClass =
    "min-w-0 flex-1 rounded-md border border-content/10 bg-content/5 px-2 py-1 text-[13px] text-content outline-none placeholder:text-content/35 focus:border-content/30";

  return (
    <div className="flex items-center gap-2 border-b border-content/5 px-3 py-2 last:border-b-0">
      <input
        ref={labelInput}
        aria-label="API key account name"
        value={label}
        placeholder="Name, for example Pay as you go"
        onChange={(event) => setLabel(event.target.value)}
        onKeyDown={onKeyDown}
        className={inputClass}
      />
      <input
        aria-label={`${provider === "claude" ? "Anthropic" : "OpenAI"} API key`}
        type="password"
        autoComplete="off"
        spellCheck={false}
        value={key}
        placeholder={API_KEY_PLACEHOLDER[provider]}
        onChange={(event) => setKey(event.target.value)}
        onKeyDown={onKeyDown}
        className={inputClass}
      />
      <SecondaryButton onClick={submit} disabled={!canSubmit}>
        Save key
      </SecondaryButton>
      <SecondaryButton onClick={onCancel}>Cancel</SecondaryButton>
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
  onToggleEnabled: (enabled: boolean) => void;
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
  onToggleEnabled,
}: AccountRowProps) {
  const isApi = isApiKeyAccount(account);
  const isEnabled = isAccountEnabled(account);
  const isSignedOut = !isApi && limits?.status === "unavailable";
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
          {isApi ? (
            <span className="rounded-sm bg-amber-500/20 px-1 text-[10px] font-semibold uppercase tracking-wide text-amber-400">
              API
            </span>
          ) : null}
        </div>
        <div className="truncate text-[12px] text-content/45">
          {account.identity ?? (isSignedOut ? "Not signed in" : "Signed in")}
          {isApi && !isEnabled ? " · off" : ""}
        </div>
      </div>
      {isApi ? (
        <div className="flex items-center gap-2">
          <span className="text-[12px] text-content/50">Enabled</span>
          <Toggle
            label={`Enable ${account.label}`}
            on={isEnabled}
            onChange={onToggleEnabled}
          />
        </div>
      ) : (
        <UsageSummary limits={limits} now={now} />
      )}
      {isSignedOut && isCustom ? (
        <SecondaryButton onClick={onSignIn}>Sign in</SecondaryButton>
      ) : null}
      <SecondaryButton onClick={onDefault} disabled={isDefault || (isApi && !isEnabled)}>
        {isDefault ? "Default" : "Use by default"}
      </SecondaryButton>
      <SecondaryButton onClick={onRename}>Rename</SecondaryButton>
      {isCustom ? (
        <SecondaryButton onClick={onRemove} danger>
          Remove
        </SecondaryButton>
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
