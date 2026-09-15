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
