import { describe, expect, it } from "vitest";
import { loginCommand } from "./accountLogin";

describe("loginCommand", () => {
  it("exports CLAUDE_CONFIG_DIR for a Claude login on unix", () => {
    expect(loginCommand("claude", "/p/w 1", false)).toBe(
      "export CLAUDE_CONFIG_DIR='/p/w 1' && claude auth login\n",
    );
  });

  it("sets CODEX_HOME in PowerShell for a Codex login on Windows", () => {
    expect(loginCommand("codex", "C:\\p\\w", true)).toBe(
      "$env:CODEX_HOME='C:\\p\\w'; codex login\n",
    );
  });

  it("escapes single quotes in the path", () => {
    expect(loginCommand("claude", "/p/o'k", false)).toBe(
      "export CLAUDE_CONFIG_DIR='/p/o'\\''k' && claude auth login\n",
    );
    expect(loginCommand("codex", "C:\\o'k", true)).toBe(
      "$env:CODEX_HOME='C:\\o''k'; codex login\n",
    );
  });
});
