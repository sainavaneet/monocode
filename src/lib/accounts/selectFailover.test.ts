import { describe, expect, it } from "vitest";
import { selectFailover, tightestUsedPercent } from "./selectFailover";
import type { Account } from "../accounts";
import type { ProviderRateLimits } from "../rateLimits";

const acct = (id: string): Account => ({
  id,
  provider: "claude",
  label: id,
  profileDir: id === "default" ? null : `/p/${id}`,
  createdAt: 0,
});

const usage = (session: number, weekly?: number): ProviderRateLimits => ({
  provider: "claude",
  status: "ok",
  session: { usedPercent: session, windowMinutes: 300, resetsAt: null },
  weekly:
    weekly == null
      ? null
      : { usedPercent: weekly, windowMinutes: 10_080, resetsAt: null },
  updatedAt: 0,
  error: null,
});

describe("tightestUsedPercent", () => {
  it("takes the larger of the two windows", () => {
    expect(tightestUsedPercent(usage(20, 70))).toBe(70);
  });

  it("is zero when usage is unknown or errored", () => {
    expect(tightestUsedPercent(undefined)).toBe(0);
    expect(tightestUsedPercent({ ...usage(90), status: "error" })).toBe(0);
  });
});

describe("selectFailover", () => {
  it("picks the other account with the lowest usage", () => {
    const picked = selectFailover({
      provider: "claude",
      accounts: [acct("default"), acct("a"), acct("b")],
      usage: { default: usage(100), a: usage(60), b: usage(20) },
      exclude: "default",
    });
    expect(picked?.id).toBe("b");
  });

  it("skips exhausted accounts and returns null when none remain", () => {
    const picked = selectFailover({
      provider: "claude",
      accounts: [acct("default"), acct("a")],
      usage: { default: usage(100), a: usage(10, 100) },
      exclude: "default",
    });
    expect(picked).toBeNull();
  });

  it("treats accounts without usage data as available", () => {
    const picked = selectFailover({
      provider: "claude",
      accounts: [acct("default"), acct("a")],
      usage: { default: usage(100) },
      exclude: "default",
    });
    expect(picked?.id).toBe("a");
  });

  it("never returns the excluded account or another provider's account", () => {
    const codex: Account = { ...acct("x"), provider: "codex" };
    const picked = selectFailover({
      provider: "claude",
      accounts: [acct("default"), codex],
      usage: {},
      exclude: "default",
    });
    expect(picked).toBeNull();
  });
});
