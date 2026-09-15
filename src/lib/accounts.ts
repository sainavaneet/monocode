import { invoke } from "@tauri-apps/api/core";
import type { HarnessId } from "./session";

export type AccountProvider = "claude" | "codex";

export type Account = {
  id: string;
  provider: AccountProvider;
  label: string;
  profileDir: string | null;
  identity?: string;
  createdAt: number;
};

/** Key inside a session's `modelSettings` that names its account. */
export const ACCOUNT_SETTING_ID = "account";
/** The user's real `~/.claude` / `~/.codex` login; sets no env. */
export const DEFAULT_ACCOUNT_ID = "default";
export const ACCOUNT_PROVIDERS: AccountProvider[] = ["claude", "codex"];

const ENV_KEY: Record<AccountProvider, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
};
const DEFAULT_KEY_PREFIX = "monocode.defaultAccount.";
const AUTO_SWITCH_KEY = "monocode.autoSwitchAccount";
export const AUTO_SWITCH_ACCOUNT_DEFAULT = true;

export function isAccountProvider(
  harness: HarnessId,
): harness is AccountProvider {
  return harness === "claude" || harness === "codex";
}

export function accountEnv(
  account: Account | undefined,
): Record<string, string> {
  if (!account?.profileDir) return {};
  return { [ENV_KEY[account.provider]]: account.profileDir };
}

export function withAccountSetting(
  settings: Record<string, string>,
  id: string,
): Record<string, string> {
  return { ...settings, [ACCOUNT_SETTING_ID]: id };
}

export function stripAccountSetting(
  settings: Record<string, string>,
): Record<string, string> {
  const { [ACCOUNT_SETTING_ID]: _omit, ...rest } = settings;
  return rest;
}

// --- store -----------------------------------------------------------------

let accounts: Account[] = [];
let version = 0;
let inflight: Promise<Account[]> | null = null;
const listeners = new Set<() => void>();

function emit() {
  version += 1;
  for (const listener of listeners) listener();
}

export function getAccountsSnapshot(): number {
  return version;
}

export function subscribeAccounts(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function listAccounts(): Account[] {
  return accounts;
}

export function accountsFor(provider: AccountProvider): Account[] {
  return accounts.filter((account) => account.provider === provider);
}

export function findAccount(
  provider: AccountProvider,
  id: string,
): Account | undefined {
  return accounts.find((a) => a.provider === provider && a.id === id);
}

function isAccount(value: unknown): value is Account {
  if (!value || typeof value !== "object") return false;
  const rec = value as Record<string, unknown>;
  return (
    typeof rec.id === "string" &&
    (rec.provider === "claude" || rec.provider === "codex") &&
    typeof rec.label === "string"
  );
}

let loadedOnce = false;

export function refreshAccounts(): Promise<Account[]> {
  if (inflight) return inflight;
  inflight = invoke<unknown>("accounts_list")
    .then((list) => {
      accounts = Array.isArray(list) ? list.filter(isAccount) : [];
      emit();
      return accounts;
    })
    .catch((error: unknown) => {
      console.error("accounts_list failed", error);
      return accounts;
    })
    .finally(() => {
      // A failed first load still counts as attempted; the UI refreshes
      // again on its own, and turns must not stall on a broken backend.
      loadedOnce = true;
      inflight = null;
    });
  return inflight;
}

export async function createAccount(
  provider: AccountProvider,
  label: string,
): Promise<Account> {
  const account = await invoke<Account>("accounts_create", {
    provider,
    label,
  });
  await refreshAccounts();
  return account;
}

export async function renameAccount(
  account: Account,
  label: string,
): Promise<void> {
  await invoke("accounts_rename", {
    id: account.id,
    provider: account.provider,
    label,
  });
  await refreshAccounts();
}

export async function removeAccount(id: string): Promise<void> {
  await invoke("accounts_remove", { id });
  await refreshAccounts();
}

export function accountLoginReady(account: Account): Promise<boolean> {
  return invoke<boolean>("accounts_login_ready", {
    id: account.id,
    provider: account.provider,
  });
}

/**
 * Resolves once the list has been loaded at least once. Adapters await this
 * before resolving a session's account, so a session pinned to a non-default
 * login never spawns under the default one just because nothing has opened
 * the picker or Settings yet.
 */
export function ensureAccountsLoaded(): Promise<void> {
  if (loadedOnce) return Promise.resolve();
  return refreshAccounts().then(() => undefined);
}

/** Test-only: reset the in-memory list. */
export function __accountsTestReset() {
  accounts = [];
  inflight = null;
  version = 0;
  loadedOnce = false;
}

// --- defaults ---------------------------------------------------------------

export function loadDefaultAccountId(provider: AccountProvider): string {
  try {
    return (
      localStorage.getItem(DEFAULT_KEY_PREFIX + provider) ?? DEFAULT_ACCOUNT_ID
    );
  } catch {
    return DEFAULT_ACCOUNT_ID;
  }
}

export function saveDefaultAccountId(provider: AccountProvider, id: string) {
  try {
    if (id === DEFAULT_ACCOUNT_ID) {
      localStorage.removeItem(DEFAULT_KEY_PREFIX + provider);
    } else {
      localStorage.setItem(DEFAULT_KEY_PREFIX + provider, id);
    }
  } catch {
    // Preferences are best-effort.
  }
  emit();
}

/**
 * The account a session should run on: its own setting when that names a
 * known account, else the provider default from Settings, else `default`.
 */
export function accountIdFor(
  provider: AccountProvider,
  settings?: Record<string, string>,
): string {
  const chosen = settings?.[ACCOUNT_SETTING_ID];
  if (
    chosen &&
    (chosen === DEFAULT_ACCOUNT_ID || findAccount(provider, chosen))
  ) {
    return chosen;
  }
  const preferred = loadDefaultAccountId(provider);
  if (preferred === DEFAULT_ACCOUNT_ID || findAccount(provider, preferred)) {
    return preferred;
  }
  return DEFAULT_ACCOUNT_ID;
}

export function accountFor(
  provider: AccountProvider,
  settings?: Record<string, string>,
): Account | undefined {
  return findAccount(provider, accountIdFor(provider, settings));
}

export function loadAutoSwitchAccount(): boolean {
  try {
    const raw = localStorage.getItem(AUTO_SWITCH_KEY);
    return raw == null ? AUTO_SWITCH_ACCOUNT_DEFAULT : raw !== "false";
  } catch {
    return AUTO_SWITCH_ACCOUNT_DEFAULT;
  }
}

export function saveAutoSwitchAccount(value: boolean) {
  try {
    localStorage.setItem(AUTO_SWITCH_KEY, value ? "true" : "false");
  } catch {
    // Preferences are best-effort.
  }
}
