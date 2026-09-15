import { useEffect, useRef, useState } from "react";
import { TerminalView } from "../TerminalView";
import { accountLoginReady, type Account } from "../../lib/accounts";
import { loginCommand, LOGIN_POLL_MS } from "../../lib/accountLogin";
import { homeDir } from "../../lib/fs";
import { killPty, writePty } from "../../lib/pty";
import { IS_WIN } from "../../lib/platform";
import { SecondaryButton } from "./controls";

/** The shell needs a moment to print its prompt before we type into it. */
const FIRST_WRITE_DELAY_MS = 600;
const WRITE_RETRY_MS = 500;
const WRITE_ATTEMPTS = 6;

type LoginStatus = "starting" | "waiting" | "ready";

interface AccountLoginTerminalProps {
  account: Account;
  onDone: () => void;
  onCancel: () => void;
}

/**
 * An embedded shell that runs the provider's own login command with the
 * account's profile directory selected, then closes itself once the
 * credential appears on disk or in the Keychain.
 */
export function AccountLoginTerminal({
  account,
  onDone,
  onCancel,
}: AccountLoginTerminalProps) {
  const ptyId = `account-login-${account.provider}-${account.id}`;
  const [cwd, setCwd] = useState<string | null>(null);
  const [status, setStatus] = useState<LoginStatus>("starting");
  const wrote = useRef(false);

  useEffect(() => {
    void homeDir().then(setCwd);
  }, []);

  useEffect(() => {
    if (!cwd || !account.profileDir || wrote.current) return;
    const command = loginCommand(account.provider, account.profileDir, IS_WIN);
    let attempts = 0;
    let timer = 0;
    const attempt = () => {
      attempts += 1;
      void writePty(ptyId, command)
        .then(() => {
          wrote.current = true;
          setStatus("waiting");
        })
        .catch((error: unknown) => {
          if (attempts >= WRITE_ATTEMPTS) {
            console.error("account login: terminal write failed", error);
            return;
          }
          timer = window.setTimeout(attempt, WRITE_RETRY_MS);
        });
    };
    timer = window.setTimeout(attempt, FIRST_WRITE_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [account.profileDir, account.provider, cwd, ptyId]);

  useEffect(() => {
    if (status !== "waiting") return;
    const poll = window.setInterval(() => {
      void accountLoginReady(account)
        .then((ready) => {
          if (!ready) return;
          window.clearInterval(poll);
          setStatus("ready");
          onDone();
        })
        .catch((error: unknown) => {
          console.error("account login: readiness check failed", error);
        });
    }, LOGIN_POLL_MS);
    return () => window.clearInterval(poll);
  }, [account, onDone, status]);

  useEffect(() => {
    return () => {
      void killPty(ptyId).catch(() => undefined);
    };
  }, [ptyId]);

  return (
    <div className="mt-3 overflow-hidden rounded-md border border-content/10">
      <div className="flex items-center justify-between gap-3 border-b border-content/10 px-3 py-1.5 text-[12px] text-content/60">
        <span>
          {status === "ready"
            ? "Signed in."
            : `Finish signing in to ${account.label} below. This panel closes once the login is saved.`}
        </span>
        <SecondaryButton onClick={onCancel}>Cancel</SecondaryButton>
      </div>
      <div className="h-72">
        {cwd ? <TerminalView id={ptyId} cwd={cwd} active /> : null}
      </div>
    </div>
  );
}
