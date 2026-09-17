// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  DEFAULT_PROVIDER_ACCOUNT_ID,
  newProviderAccount,
  providerAccountLabel,
  providerAccounts,
  removeProviderAccount,
  renameProviderAccount,
  saveProviderAccount,
  selectedProviderAccountId,
  selectProviderAccount,
} from "./providerAccounts";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => void values.delete(key),
    setItem: (key, value) => void values.set(key, String(value)),
  };
}

beforeEach(() => {
  vi.stubGlobal("localStorage", memoryStorage());
  localStorage.clear();
  vi.restoreAllMocks();
});

describe("provider accounts", () => {
  it("always exposes the provider-owned default account", () => {
    expect(providerAccounts("claude")).toEqual([
      {
        id: DEFAULT_PROVIDER_ACCOUNT_ID,
        provider: "claude",
        label: "Default account",
        isDefault: true,
      },
    ]);
  });

  it("stores named profiles separately per provider", () => {
    vi.spyOn(crypto, "randomUUID").mockReturnValue(
      "00000000-0000-4000-8000-000000000001",
    );
    const work = newProviderAccount("claude", "  Work   account  ");
    saveProviderAccount(work);

    expect(providerAccounts("claude").map((account) => account.label)).toEqual([
      "Default account",
      "Work account",
    ]);
    expect(providerAccounts("codex")).toHaveLength(1);
    expect(providerAccountLabel("claude", work.id)).toBe("Work account");
  });

  it("falls back to the default account for malformed stored profiles", () => {
    localStorage.setItem(
      "monocode.providerAccounts.v1",
      JSON.stringify({ claude: { id: "not-an-array" } }),
    );

    expect(providerAccounts("claude").map((account) => account.id)).toEqual([
      DEFAULT_PROVIDER_ACCOUNT_ID,
    ]);
  });

  it("replaces malformed provider storage when saving an account", () => {
    localStorage.setItem(
      "monocode.providerAccounts.v1",
      JSON.stringify({ codex: "not-an-array" }),
    );

    expect(() =>
      saveProviderAccount({
        id: "account-work",
        provider: "codex",
        label: "Work",
      }),
    ).not.toThrow();
    expect(providerAccounts("codex").map((account) => account.label)).toEqual([
      "Default account",
      "Work",
    ]);
  });

  it("remembers a selection per project and ignores unknown ids", () => {
    const work = {
      id: "account-work",
      provider: "codex" as const,
      label: "Work",
    };
    saveProviderAccount(work);
    selectProviderAccount("codex", "/repo/one", work.id);

    expect(selectedProviderAccountId("codex", "/repo/one")).toBe(work.id);
    expect(selectedProviderAccountId("codex", "/repo/two")).toBe(
      DEFAULT_PROVIDER_ACCOUNT_ID,
    );
    selectProviderAccount("codex", "/repo/one", "missing");
    expect(selectedProviderAccountId("codex", "/repo/one")).toBe(work.id);
  });

  it("renames a non-default account", () => {
    saveProviderAccount({
      id: "account-work",
      provider: "claude",
      label: "Work",
    });
    expect(
      renameProviderAccount("claude", "account-work", "  Team   login "),
    ).toBe(true);
    expect(providerAccountLabel("claude", "account-work")).toBe("Team login");
    expect(renameProviderAccount("claude", "default", "Other")).toBe(false);
  });

  it("removes an account and clears every project selection", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    saveProviderAccount({
      id: "account-work",
      provider: "codex",
      label: "Work",
    });
    selectProviderAccount("codex", "/repo/one", "account-work");
    selectProviderAccount("codex", "/repo/two", "account-work");

    await expect(removeProviderAccount("codex", "account-work")).resolves.toBe(
      true,
    );

    expect(invoke).toHaveBeenCalledWith("provider_account_remove", {
      provider: "codex",
      accountId: "account-work",
    });
    expect(providerAccounts("codex")).toHaveLength(1);
    expect(selectedProviderAccountId("codex", "/repo/one")).toBe("default");
    expect(selectedProviderAccountId("codex", "/repo/two")).toBe("default");
  });
});
