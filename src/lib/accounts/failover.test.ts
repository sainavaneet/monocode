import { beforeEach, describe, expect, it, vi } from "vitest";
import type { HarnessEvent } from "../harness/types";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  fetchClaude: vi.fn(),
  fetchCodex: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("../rateLimitsFetch", () => ({
  fetchClaudeRateLimits: mocks.fetchClaude,
  fetchCodexRateLimits: mocks.fetchCodex,
}));

import {
  __accountsTestReset,
  refreshAccounts,
  saveAutoSwitchAccount,
  type Account,
} from "../accounts";
import type { ProviderRateLimits } from "../rateLimits";
import { chooseFailoverAccount, sendTurnWithFailover } from "./failover";

function mockLocalStorage() {
  const data = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    value: {
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
    },
    configurable: true,
  });
}

const dflt: Account = {
  id: "default",
  provider: "claude",
  label: "Default",
  profileDir: null,
  createdAt: 0,
};
const work: Account = {
  id: "w1",
  provider: "claude",
  label: "Work",
  profileDir: "/p/w1",
  identity: "w@x.y",
  createdAt: 1,
};

const usage = (used: number): ProviderRateLimits => ({
  provider: "claude",
  status: "ok",
  session: { usedPercent: used, windowMinutes: 300, resetsAt: null },
  weekly: null,
  updatedAt: 0,
  error: null,
});

function turnInput(events: HarnessEvent[]) {
  return {
    harness: "claude" as const,
    sessionId: "s1",
    cwd: "/repo",
    model: "claude:claude-sonnet-5",
    modelSettings: { effort: "high" },
    runtimeMode: "supervised" as const,
    text: "hello",
    attachments: [],
    onEvent: (event: HarnessEvent) => {
      events.push(event);
    },
  };
}

beforeEach(async () => {
  mockLocalStorage();
  mocks.invoke.mockReset();
  mocks.fetchClaude.mockReset();
  mocks.fetchCodex.mockReset();
  __accountsTestReset();
  mocks.invoke.mockResolvedValue([dflt, work]);
  await refreshAccounts();
});

describe("chooseFailoverAccount", () => {
  it("picks the other account with quota", async () => {
    mocks.fetchClaude.mockImplementation(async (dir: string | null) =>
      usage(dir == null ? 100 : 30),
    );
    const next = await chooseFailoverAccount("claude", "default");
    expect(next?.id).toBe("w1");
    expect(mocks.fetchClaude).toHaveBeenCalledWith(null);
    expect(mocks.fetchClaude).toHaveBeenCalledWith("/p/w1");
  });

  it("returns null when the current account is the only one", async () => {
    mocks.invoke.mockResolvedValue([dflt]);
    __accountsTestReset();
    await refreshAccounts();
    expect(await chooseFailoverAccount("claude", "default")).toBeNull();
  });
});

describe("sendTurnWithFailover", () => {
  it("retries once on the next account and rebinds the session", async () => {
    mocks.fetchClaude.mockImplementation(async (dir: string | null) =>
      usage(dir == null ? 100 : 10),
    );
    const events: HarnessEvent[] = [];
    const send = vi.fn(async (input: { modelSettings?: Record<string, string>; onEvent: (e: HarnessEvent) => void }) => {
      if (input.modelSettings?.account !== "w1") {
        input.onEvent({ type: "message.delta", text: "You've hit your limit" });
        input.onEvent({ type: "session.error", message: "usage_limit_reached" });
        return;
      }
      input.onEvent({ type: "message.delta", text: "done" });
    });

    await sendTurnWithFailover(turnInput(events), send);

    expect(send).toHaveBeenCalledTimes(2);
    expect(send.mock.calls[1][0].modelSettings).toEqual({
      effort: "high",
      account: "w1",
    });
    expect(events.map((event) => event.type)).toEqual([
      "message.delta",
      "status",
      "session.configChanged",
      "message.delta",
    ]);
    expect(events[1]).toMatchObject({
      text: "Switched to Work · w@x.y after a rate limit",
    });
    expect(events[2]).toMatchObject({
      modelSettings: { effort: "high", account: "w1" },
    });
  });

  it("delivers the original error when no account has quota", async () => {
    mocks.fetchClaude.mockResolvedValue(usage(100));
    const events: HarnessEvent[] = [];
    const send = vi.fn(async (input: { onEvent: (e: HarnessEvent) => void }) => {
      input.onEvent({ type: "session.error", message: "Rate limit exceeded" });
    });

    await sendTurnWithFailover(turnInput(events), send);

    expect(send).toHaveBeenCalledTimes(1);
    expect(events).toEqual([
      { type: "session.error", message: "Rate limit exceeded" },
    ]);
  });

  it("rethrows a thrown rate-limit failure when nothing can take over", async () => {
    mocks.fetchClaude.mockResolvedValue(usage(100));
    const send = vi.fn(async () => {
      throw new Error("429 Too Many Requests");
    });
    await expect(sendTurnWithFailover(turnInput([]), send)).rejects.toThrow(
      "429",
    );
    expect(send).toHaveBeenCalledTimes(1);
  });

  it("retries after a thrown rate-limit failure when another account has quota", async () => {
    mocks.fetchClaude.mockImplementation(async (dir: string | null) =>
      usage(dir == null ? 100 : 0),
    );
    const send = vi.fn(async (input: { modelSettings?: Record<string, string> }) => {
      if (input.modelSettings?.account !== "w1") {
        throw new Error("You've hit your limit");
      }
    });
    await sendTurnWithFailover(turnInput([]), send);
    expect(send).toHaveBeenCalledTimes(2);
  });

  it("passes other failures through untouched", async () => {
    const events: HarnessEvent[] = [];
    const send = vi.fn(async (input: { onEvent: (e: HarnessEvent) => void }) => {
      input.onEvent({ type: "session.error", message: "Claude Code exited" });
    });
    await sendTurnWithFailover(turnInput(events), send);
    expect(send).toHaveBeenCalledTimes(1);
    expect(mocks.fetchClaude).not.toHaveBeenCalled();
    expect(events).toEqual([
      { type: "session.error", message: "Claude Code exited" },
    ]);
  });

  it("does nothing special when the setting is off or the provider has no accounts", async () => {
    saveAutoSwitchAccount(false);
    const events: HarnessEvent[] = [];
    const send = vi.fn(async (input: { onEvent: (e: HarnessEvent) => void }) => {
      input.onEvent({ type: "session.error", message: "usage_limit_reached" });
    });
    await sendTurnWithFailover(turnInput(events), send);
    expect(send).toHaveBeenCalledTimes(1);
    expect(events).toHaveLength(1);

    saveAutoSwitchAccount(true);
    const cursor = vi.fn(async () => undefined);
    await sendTurnWithFailover(
      { ...turnInput([]), harness: "cursor" as const },
      cursor,
    );
    expect(cursor).toHaveBeenCalledTimes(1);
  });
});
