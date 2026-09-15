import { describe, expect, it } from "vitest";
import { accountModelSetting } from "./accountSetting";
import type { Account } from "./accounts";

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
  profileDir: "/p",
  identity: "w@x.y",
  createdAt: 1,
};
const codex: Account = {
  id: "x1",
  provider: "codex",
  label: "Codex",
  profileDir: "/q",
  createdAt: 1,
};

describe("accountModelSetting", () => {
  it("returns null with a single account for the provider", () => {
    expect(accountModelSetting("claude", [dflt, codex], "default")).toBeNull();
  });

  it("builds a select with labels and identity", () => {
    const setting = accountModelSetting("claude", [dflt, work, codex], "w1");
    expect(setting).toMatchObject({
      id: "account",
      kind: "select",
      value: "w1",
    });
    expect(setting?.options).toEqual([
      { value: "default", label: "Default" },
      { value: "w1", label: "Work · w@x.y" },
    ]);
  });
});
