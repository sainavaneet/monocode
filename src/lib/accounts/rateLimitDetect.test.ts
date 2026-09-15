import { describe, expect, it } from "vitest";
import { isRateLimitFailure } from "./rateLimitDetect";

describe("isRateLimitFailure", () => {
  it.each([
    "You've hit your limit · resets 3pm",
    "usage_limit_reached",
    "Rate limit exceeded, retry after 30s",
    "HTTP 429 Too Many Requests",
    "You have reached your usage limit",
    "quota exceeded for this account",
    "rate-limited by upstream",
  ])("detects %s", (message) => {
    expect(isRateLimitFailure(message)).toBe(true);
  });

  it.each([
    "Claude Code exited",
    "Working directory does not exist",
    "Codex app-server exited",
    "",
    null,
    undefined,
  ])("ignores %s", (message) => {
    expect(isRateLimitFailure(message)).toBe(false);
  });
});
