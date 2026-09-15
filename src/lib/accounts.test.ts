import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

import {
  __accountsTestReset,
  accountEnv,
  accountFor,
  accountIdFor,
  ACCOUNT_SETTING_ID,
  accountLoginReady,
  accountsFor,
  loadAutoSwitchAccount,
  refreshAccounts,
  saveAutoSwitchAccount,
  saveDefaultAccountId,
  stripAccountSetting,
  usableAccountsFor,
  accountRef,
  withAccountSetting,
  type Account,
} from "./accounts";

function mockLocalStorage() {
  const data = new Map<string, string>();
  const storage = {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => {
      data.set(key, value);
    },
    removeItem: (key: string) => {
      data.delete(key);
    },
    clear: () => data.clear(),
    key: (index: number) => [...data.keys()][index] ?? null,
    get length() {
      return data.size;
    },
  };
  Object.defineProperty(globalThis, "localStorage", {
    value: storage,
    configurable: true,
  });
}

const work: Account = {
  id: "w1",
  provider: "claude",
  label: "Work",
  profileDir: "/p/w1",
  createdAt: 1,
};
const dflt: Account = {
  id: "default",
  provider: "claude",
  label: "Default",
  profileDir: null,
  createdAt: 0,
};

beforeEach(() => {
  mockLocalStorage();
  mocks.invoke.mockReset();
  __accountsTestReset();
});

describe("accountEnv", () => {
  it("sets no env for the default account", () => {
    expect(accountEnv(dflt)).toEqual({});
    expect(accountEnv(undefined)).toEqual({});
  });

  it("sets CLAUDE_CONFIG_DIR for a Claude profile", () => {
    expect(accountEnv(work)).toEqual({ CLAUDE_CONFIG_DIR: "/p/w1" });
  });

  it("sets CODEX_HOME for a Codex profile", () => {
    expect(accountEnv({ ...work, provider: "codex" })).toEqual({
      CODEX_HOME: "/p/w1",
    });
  });
});

describe("refreshAccounts", () => {
  it("loads accounts and drops malformed rows", async () => {
    mocks.invoke.mockResolvedValueOnce([dflt, work, { nope: true }]);
    await refreshAccounts();
    expect(accountsFor("claude").map((a) => a.id)).toEqual(["default", "w1"]);
    expect(mocks.invoke).toHaveBeenCalledWith("accounts_list");
  });

  it("keeps the previous list when the call fails", async () => {
    mocks.invoke.mockResolvedValueOnce([work]);
    await refreshAccounts();
    mocks.invoke.mockRejectedValueOnce(new Error("boom"));
    const list = await refreshAccounts();
    expect(list).toEqual([work]);
  });
});

describe("accountIdFor", () => {
  it("returns the setting when it names a known account", async () => {
    mocks.invoke.mockResolvedValueOnce([dflt, work]);
    await refreshAccounts();
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "w1" })).toBe("w1");
    expect(accountFor("claude", { [ACCOUNT_SETTING_ID]: "w1" })).toEqual(work);
  });

  it("falls back to the saved default when the setting is unknown", async () => {
    mocks.invoke.mockResolvedValueOnce([dflt, work]);
    await refreshAccounts();
    saveDefaultAccountId("claude", "w1");
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "gone" })).toBe(
      "w1",
    );
  });

  it("falls back to default when nothing is set", () => {
    expect(accountIdFor("claude", {})).toBe("default");
    expect(accountIdFor("claude", undefined)).toBe("default");
  });

  it("ignores a saved default that no longer exists", () => {
    saveDefaultAccountId("claude", "removed");
    expect(accountIdFor("claude", {})).toBe("default");
  });
});

describe("settings helpers", () => {
  it("withAccountSetting adds without mutating", () => {
    const base = { effort: "high" };
    const next = withAccountSetting(base, "w1");
    expect(next).toEqual({ effort: "high", account: "w1" });
    expect(base).toEqual({ effort: "high" });
  });

  it("stripAccountSetting removes the key", () => {
    expect(stripAccountSetting({ effort: "high", account: "w1" })).toEqual({
      effort: "high",
    });
  });
});

describe("accountLoginReady", () => {
  it("passes id and provider", async () => {
    mocks.invoke.mockResolvedValueOnce(true);
    await expect(accountLoginReady(work)).resolves.toBe(true);
    expect(mocks.invoke).toHaveBeenCalledWith("accounts_login_ready", {
      id: "w1",
      provider: "claude",
    });
  });
});

describe("auto switch", () => {
  it("defaults to on and persists off", () => {
    expect(loadAutoSwitchAccount()).toBe(true);
    saveAutoSwitchAccount(false);
    expect(loadAutoSwitchAccount()).toBe(false);
  });
});

describe("ensureAccountsLoaded", () => {
  it("loads once and then resolves without another call", async () => {
    mocks.invoke.mockResolvedValue([dflt, work]);
    const { ensureAccountsLoaded } = await import("./accounts");
    await ensureAccountsLoaded();
    await ensureAccountsLoaded();
    expect(mocks.invoke).toHaveBeenCalledTimes(1);
    expect(accountsFor("claude")).toHaveLength(2);
  });
});

describe("renameAccount", () => {
  it("passes id, provider and label so the default can be renamed too", async () => {
    mocks.invoke.mockResolvedValue([]);
    const { renameAccount } = await import("./accounts");
    await renameAccount(dflt, "Personal");
    expect(mocks.invoke).toHaveBeenCalledWith("accounts_rename", {
      id: "default",
      provider: "claude",
      label: "Personal",
    });
  });
});

describe("API-key accounts", () => {
  const apiKey: Account = {
    id: "k1",
    provider: "claude",
    label: "Key",
    profileDir: "/p/k1",
    createdAt: 2,
    kind: "apiKey",
    enabled: false,
    hint: "7890",
  };

  it("hides disabled accounts from usable list and falls back for them", async () => {
    mocks.invoke.mockResolvedValueOnce([dflt, work, apiKey]);
    await refreshAccounts();
    expect(usableAccountsFor("claude").map((a) => a.id)).toEqual([
      "default",
      "w1",
    ]);
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "k1" })).toBe(
      "default",
    );
  });

  it("uses an enabled API-key account when chosen", async () => {
    mocks.invoke.mockResolvedValueOnce([dflt, { ...apiKey, enabled: true }]);
    await refreshAccounts();
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "k1" })).toBe("k1");
    expect(accountRef(accountFor("claude", { account: "k1" }))).toEqual({
      provider: "claude",
      id: "k1",
    });
    expect(accountRef(dflt)).toBeUndefined();
  });

  it("creates and toggles through the backend", async () => {
    mocks.invoke.mockResolvedValue([]);
    const { createApiKeyAccount, setAccountEnabled } = await import(
      "./accounts"
    );
    await createApiKeyAccount("codex", "Key", "sk-x");
    expect(mocks.invoke).toHaveBeenCalledWith("accounts_create_api_key", {
      provider: "codex",
      label: "Key",
      key: "sk-x",
    });
    await setAccountEnabled(apiKey, true);
    expect(mocks.invoke).toHaveBeenCalledWith("accounts_set_enabled", {
      id: "k1",
      provider: "claude",
      enabled: true,
    });
  });
});
