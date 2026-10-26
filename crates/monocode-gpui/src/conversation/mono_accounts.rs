//! Account recovery for a Mono's own chat. A Mono is a Conversation, so it
//! reuses the in-place switch (`switch_usage_account`, which carries the
//! provider thread when it can); what this adds are the Mono's rules: only
//! accounts on the Mono's (or project's) allowlist, never one that is cooling
//! down or needs to sign in again (also per the usage cache), every account
//! sharing a spent quota rests together, and configuration or policy errors
//! never switch. Without an allowlist the chat keeps today's behaviour: it
//! waits on its own account for the user. Ordinary conversations are never
//! touched.
use super::*;
use monocode_core::accounts::eligibility::{self, AccountPlan, AllowedAccount, Pool};
use monocode_core::store::Store;
use monocode_core::usage::{Failure, Limit};

/// What a Mono's chat should do after a failed turn on `current`. None when
/// the Mono has no allowlist (the default) or the failure is not one a switch
/// can help with. Records the failure's effect on the account first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mono_plan(
    store: &Store,
    mono: &str,
    project: &str,
    kind: Kind,
    current: &str,
    failure: Failure,
    resets_at: Option<u64>,
    exhausted_until: &dyn Fn(&AllowedAccount) -> Option<u64>,
    now: u64,
) -> Result<Option<AccountPlan>, String> {
    if !matches!(
        failure,
        Failure::UsageExhausted | Failure::SharedQuota | Failure::Auth
    ) {
        return Ok(None);
    }
    let Some(allowlist) = eligibility::allowlist_for(store, Some(mono), project)? else {
        return Ok(None);
    };
    let failed = allowlist.entry(kind.id(), current);
    eligibility::record_failure(store, &allowlist, &failed, failure, resets_at, now)?;
    let health = eligibility::health(store)?;
    let exists = eligibility::known_accounts(store)?;
    // A chat stays on its provider: only accounts of the same one.
    let mut same = allowlist.clone();
    same.accounts.retain(|a| a.provider == kind.id());
    same.provider_fallback = false;
    let pool = Pool {
        allowlist: &same,
        health: &health,
        exists: &exists,
        exhausted_until,
    };
    Ok(Some(eligibility::plan(
        failure, &failed, &pool, resets_at, now,
    )))
}

impl Conversation {
    /// After a Mono turn failed with a usage, shared-quota or sign-in error,
    /// move the chat to the next eligible allowed account, or wait or block
    /// with the reason.
    pub(super) fn recover_mono_account(&mut self, cx: &mut Context<Self>) {
        let Some(mono) = self.settings.mono.as_ref().map(|m| m.mono.id.clone()) else {
            return;
        };
        if self.busy || self.stopping || self.account_switching || self.is_remote() {
            return;
        }
        let failure = match Failure::classify(&self.status) {
            Failure::SharedQuota => Failure::SharedQuota,
            _ if self.settings.usage_limit.is_some() => Failure::UsageExhausted,
            other => other,
        };
        if !matches!(
            failure,
            Failure::UsageExhausted | Failure::SharedQuota | Failure::Auth
        ) {
            return;
        }
        let kind = Kind::from_id(&self.summary.provider);
        let now = monocode_core::time::now_ms();
        let resets_at = self
            .settings
            .usage_limit
            .as_ref()
            .and_then(|l| l.resets_at)
            .and_then(|at| u64::try_from(at).ok());
        // The usage cache keeps out accounts its snapshots show exhausted.
        let center = crate::accounts::get(cx);
        let model = self.account_model().map(str::to_owned);
        let exhausted: std::collections::HashMap<String, u64> = center
            .as_ref()
            .map(|center| {
                let center = center.read(cx);
                center
                    .book
                    .list(kind)
                    .into_iter()
                    .filter_map(|account| {
                        let key = crate::accounts::Key::new(
                            kind,
                            center.profile(&account.id),
                            model.as_deref(),
                        );
                        let snapshot = center.usage.get(&key)?.snapshot.as_ref()?;
                        let spent = snapshot.headroom(now as i64) == Some(0.);
                        let reset = snapshot.exhausted_reset()?;
                        spent.then(|| (account.id, u64::try_from(reset).unwrap_or(0)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let exhausted_until = |a: &AllowedAccount| exhausted.get(&a.account_id).copied();
        let current = self.settings.account.id.clone();
        let project = self.summary.cwd.clone();
        let plan = locked(&self.store).and_then(|store| {
            mono_plan(
                &store,
                &mono,
                &project,
                kind,
                &current,
                failure,
                resets_at,
                &exhausted_until,
                now,
            )
        });
        match plan {
            Ok(Some(AccountPlan::Switch(next))) => {
                let profile = center
                    .map(|center| center.read(cx).profile(&next.account_id))
                    .unwrap_or_else(|| monocode_core::accounts::Profile {
                        id: next.account_id.clone(),
                        root: None,
                    });
                // A sign-in failure resumes through the same path as a limit.
                if self.settings.usage_limit.is_none() {
                    self.settings.usage_limit = Some(Limit::default());
                }
                self.switch_usage_account(profile, cx);
            }
            Ok(Some(AccountPlan::Wait { until })) => {
                let wait = monocode_core::usage::duration(
                    i64::try_from(until.saturating_sub(now)).unwrap_or(0),
                );
                self.status = match (&mut self.settings.usage_limit, failure) {
                    // A usage limit resumes itself at the reset.
                    (Some(limit), Failure::UsageExhausted | Failure::SharedQuota) => {
                        limit.resets_at = Some(i64::try_from(until).unwrap_or(i64::MAX));
                        limit.resume_at_reset = true;
                        format!(
                            "No allowed account is available until a limit resets in {wait}; this chat retries on its account then."
                        )
                    }
                    // Nothing resumes a sign-in failure by itself.
                    _ => format!(
                        "This account needs to sign in again. Sign in to resume, or choose another account (another allowed account's limit resets in {wait})."
                    ),
                };
                self.save_settings(cx);
                cx.notify();
            }
            Ok(Some(AccountPlan::Block(reason))) => {
                self.status = reason;
                cx.notify();
            }
            Ok(Some(AccountPlan::Same) | None) => {}
            Err(error) => eprintln!("Mono account recovery: {error}"),
        }
    }
}

#[cfg(test)]
#[path = "mono_accounts_tests.rs"]
mod tests;
