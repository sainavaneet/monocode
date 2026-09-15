import type { AccountProvider } from "./accounts";

const ENV_KEY: Record<AccountProvider, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
};
const LOGIN: Record<AccountProvider, string> = {
  claude: "claude auth login",
  codex: "codex login",
};

/** How often the login panel asks whether the credential has appeared. */
export const LOGIN_POLL_MS = 2000;

function shQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function psQuote(value: string): string {
  return `'${value.replace(/'/g, "''")}'`;
}

/**
 * The line typed into the login terminal. It points the CLI at the account's
 * profile directory for this shell only, then starts the CLI's own login.
 * Ends with a newline so the shell runs it.
 */
export function loginCommand(
  provider: AccountProvider,
  profileDir: string,
  isWindows: boolean,
): string {
  if (isWindows) {
    return `$env:${ENV_KEY[provider]}=${psQuote(profileDir)}; ${LOGIN[provider]}\n`;
  }
  return `export ${ENV_KEY[provider]}=${shQuote(profileDir)} && ${LOGIN[provider]}\n`;
}
