//! Which accounts recovery may use, and their standing after failures.
//!
//! An allowlist (per Mono, else per project) names the accounts work may move
//! to, by priority, with an optional quota group for accounts that share one
//! organization or workspace limit. Health records cooldowns (until a usage
//! limit resets) and accounts whose credentials failed (until they sign in
//! again). Both live in `native_settings` and hold account ids only, never a
//! key or token. The app's usage cache is layered on top through
//! [`Pool::exhausted_until`], so a snapshot that shows an exhausted window
//! also keeps the account out.
use super::{project_key, validate_id, DEFAULT};
use crate::store::Store;
use crate::usage::Failure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How long an exhausted account rests when its reset time is unknown.
pub const DEFAULT_COOLDOWN_MS: u64 = 60 * 60 * 1000;
const ALLOWLIST_KEY: &str = "native-account-allowlist";
const HEALTH_KEY: &str = "native-account-health";

/// One account work may run on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedAccount {
    pub provider: String,
    pub account_id: String,
    /// Lower runs first.
    #[serde(default)]
    pub priority: i64,
    /// Accounts with the same group share one organization or workspace
    /// quota: when it is spent, all of them rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_group: Option<String>,
}

impl AllowedAccount {
    pub fn new(provider: &str, account_id: &str) -> Self {
        Self {
            provider: provider.into(),
            account_id: account_id.into(),
            priority: 0,
            quota_group: None,
        }
    }

    fn label(&self) -> String {
        format!("{}/{}", self.provider, self.account_id)
    }

    fn same(&self, other: &Self) -> bool {
        self.provider == other.provider && self.account_id == other.account_id
    }
}

/// The accounts recovery may use, and whether it may change provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Allowlist {
    pub accounts: Vec<AllowedAccount>,
    #[serde(default)]
    pub provider_fallback: bool,
}

impl Allowlist {
    /// The default: only the account the work started on.
    pub fn only(provider: &str, account_id: &str) -> Self {
        Self {
            accounts: vec![AllowedAccount::new(provider, account_id)],
            provider_fallback: false,
        }
    }

    /// Accounts by priority; equal priorities keep their listed order.
    pub fn ordered(&self) -> Vec<&AllowedAccount> {
        let mut accounts: Vec<_> = self.accounts.iter().collect();
        accounts.sort_by_key(|a| a.priority);
        accounts
    }

    /// The entry for `account`, or the account itself when it is not listed.
    pub fn entry(&self, provider: &str, account_id: &str) -> AllowedAccount {
        self.accounts
            .iter()
            .find(|a| a.provider == provider && a.account_id == account_id)
            .cloned()
            .unwrap_or_else(|| AllowedAccount::new(provider, account_id))
    }

    /// `account` and every listed account sharing its quota group.
    pub fn sharing(&self, account: &AllowedAccount) -> Vec<AllowedAccount> {
        let mut shared = vec![account.clone()];
        if let Some(group) = &account.quota_group {
            for other in &self.accounts {
                if other.quota_group.as_ref() == Some(group) && !other.same(account) {
                    shared.push(other.clone());
                }
            }
        }
        shared
    }

    fn validate(&self) -> Result<(), String> {
        for account in &self.accounts {
            if crate::provider::Kind::parse(&account.provider).is_none() {
                return Err(format!("Unknown provider {}", account.provider));
            }
            if account.account_id != DEFAULT {
                validate_id(&account.account_id)?;
            }
        }
        Ok(())
    }
}

/// Where an allowlist applies.
#[derive(Clone, Copy, Debug)]
pub enum Scope<'a> {
    Mono(&'a str),
    Project(&'a str),
}

fn allowlist_key(scope: Scope<'_>) -> String {
    match scope {
        Scope::Mono(id) => format!("{ALLOWLIST_KEY}:mono:{id}"),
        Scope::Project(path) => format!("{ALLOWLIST_KEY}:project:{}", project_key(path)),
    }
}

pub fn allowlist(store: &Store, scope: Scope<'_>) -> Result<Option<Allowlist>, String> {
    match store.setting(&allowlist_key(scope))? {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("The saved account allowlist is unreadable: {e}")),
        _ => Ok(None),
    }
}

pub fn save_allowlist(store: &Store, scope: Scope<'_>, list: &Allowlist) -> Result<(), String> {
    list.validate()?;
    let text = serde_json::to_string(list).map_err(|e| e.to_string())?;
    store.save_setting(&allowlist_key(scope), &text)
}

/// The Mono's own allowlist, else the project's; None means the default
/// (only the account the work started on).
pub fn allowlist_for(
    store: &Store,
    mono: Option<&str>,
    project: &str,
) -> Result<Option<Allowlist>, String> {
    if let Some(mono) = mono {
        if let Some(list) = allowlist(store, Scope::Mono(mono))? {
            return Ok(Some(list));
        }
    }
    allowlist(store, Scope::Project(project))
}

/// An account's standing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Health {
    /// Unix ms until which the account rests (a usage or shared limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<u64>,
    /// Whether `cooldown_until` is the provider's reported reset, rather
    /// than [`DEFAULT_COOLDOWN_MS`]; only a known reset is worth waiting for.
    pub reset_known: bool,
    /// Its credentials failed; it stays out until it signs in again.
    pub unhealthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub updated_at: u64,
}

impl Health {
    pub fn cooling(until: u64, failure: Failure, now: u64) -> Self {
        Self {
            cooldown_until: Some(until),
            reset_known: true,
            unhealthy: false,
            failure: Some(failure),
            updated_at: now,
        }
    }

    pub fn unhealthy(now: u64) -> Self {
        Self {
            cooldown_until: None,
            reset_known: false,
            unhealthy: true,
            failure: Some(Failure::Auth),
            updated_at: now,
        }
    }

    pub fn available(&self, now: u64) -> bool {
        !self.unhealthy && self.cooldown_until.is_none_or(|until| until <= now)
    }
}

/// Every account's standing, keyed `provider/account`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthBook(BTreeMap<String, Health>);

impl HealthBook {
    pub fn get(&self, provider: &str, account_id: &str) -> Health {
        self.0
            .get(&format!("{provider}/{account_id}"))
            .cloned()
            .unwrap_or_default()
    }

    pub fn set(&mut self, provider: &str, account_id: &str, health: Health) {
        self.0.insert(format!("{provider}/{account_id}"), health);
    }
}

pub fn health(store: &Store) -> Result<HealthBook, String> {
    match store.setting(HEALTH_KEY)? {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| format!("The saved account health is unreadable: {e}")),
        _ => Ok(HealthBook::default()),
    }
}

fn update(store: &Store, change: impl FnOnce(&mut HealthBook)) -> Result<(), String> {
    let mut book = health(store)?;
    change(&mut book);
    let text = serde_json::to_string(&book).map_err(|e| e.to_string())?;
    store.save_setting(HEALTH_KEY, &text)
}

/// Rests the account until its known reset `until`; a longer cooldown
/// already recorded wins.
pub fn cool_down(
    store: &Store,
    provider: &str,
    account_id: &str,
    until: u64,
    failure: Failure,
    now: u64,
) -> Result<(), String> {
    rest(store, provider, account_id, (until, true), failure, now)
}

fn rest(
    store: &Store,
    provider: &str,
    account_id: &str,
    (until, known): (u64, bool),
    failure: Failure,
    now: u64,
) -> Result<(), String> {
    update(store, |book| {
        let mut health = book.get(provider, account_id);
        let current = health.cooldown_until.filter(|at| *at > now);
        if current.is_none_or(|at| at <= until) {
            health.cooldown_until = Some(until);
            health.reset_known = known;
        }
        health.failure = Some(failure);
        health.updated_at = now;
        book.set(provider, account_id, health);
    })
}

pub fn mark_unhealthy(
    store: &Store,
    provider: &str,
    account_id: &str,
    now: u64,
) -> Result<(), String> {
    update(store, |book| {
        let mut health = book.get(provider, account_id);
        health.unhealthy = true;
        health.failure = Some(Failure::Auth);
        health.updated_at = now;
        book.set(provider, account_id, health);
    })
}

/// The account signed in again. Writes nothing unless it was unhealthy.
pub fn mark_healthy(
    store: &Store,
    provider: &str,
    account_id: &str,
    now: u64,
) -> Result<(), String> {
    if !health(store)?.get(provider, account_id).unhealthy {
        return Ok(());
    }
    update(store, |book| {
        let mut health = book.get(provider, account_id);
        health.unhealthy = false;
        health.updated_at = now;
        book.set(provider, account_id, health);
    })
}

/// Records what the failure says about the account: an exhausted account
/// rests until its reset (or [`DEFAULT_COOLDOWN_MS`]), a shared quota rests
/// every account sharing it, bad credentials make it unhealthy. Other
/// failures say nothing about the account.
pub fn record_failure(
    store: &Store,
    allowlist: &Allowlist,
    failed: &AllowedAccount,
    failure: Failure,
    resets_at: Option<u64>,
    now: u64,
) -> Result<(), String> {
    let until = match resets_at.filter(|at| *at > now) {
        Some(at) => (at, true),
        None => (now.saturating_add(DEFAULT_COOLDOWN_MS), false),
    };
    match failure {
        Failure::UsageExhausted => rest(
            store,
            &failed.provider,
            &failed.account_id,
            until,
            failure,
            now,
        ),
        Failure::SharedQuota => {
            for account in allowlist.sharing(failed) {
                rest(
                    store,
                    &account.provider,
                    &account.account_id,
                    until,
                    failure,
                    now,
                )?;
            }
            Ok(())
        }
        Failure::Auth => mark_unhealthy(store, &failed.provider, &failed.account_id, now),
        _ => Ok(()),
    }
}

/// Whether the app still knows an account (it may have been removed).
pub fn known_accounts(store: &Store) -> Result<impl Fn(&AllowedAccount) -> bool + 'static, String> {
    let book = store.provider_accounts()?;
    Ok(move |account: &AllowedAccount| {
        crate::provider::Kind::parse(&account.provider)
            .is_some_and(|kind| book.exists(kind, &account.account_id))
    })
}

/// The candidates and what is known about them.
pub struct Pool<'a> {
    pub allowlist: &'a Allowlist,
    pub health: &'a HealthBook,
    /// Whether the account still exists (it may have been removed).
    pub exists: &'a dyn Fn(&AllowedAccount) -> bool,
    /// When the app's usage cache shows the account exhausted, until when.
    pub exhausted_until: &'a dyn Fn(&AllowedAccount) -> Option<u64>,
}

impl Pool<'_> {
    fn eligible(&self, account: &AllowedAccount, now: u64) -> bool {
        (self.exists)(account)
            && self
                .health
                .get(&account.provider, &account.account_id)
                .available(now)
            && (self.exhausted_until)(account).is_none_or(|until| until <= now)
    }

    /// Why `account` is out, for a blocked reason.
    fn why_not(&self, account: &AllowedAccount, now: u64) -> String {
        let health = self.health.get(&account.provider, &account.account_id);
        let what = if !(self.exists)(account) {
            "was removed".to_owned()
        } else if health.unhealthy {
            "needs to sign in again".to_owned()
        } else if let Some(until) = health.cooldown_until.filter(|at| *at > now) {
            format!("is cooling down until {until}")
        } else if let Some(until) = (self.exhausted_until)(account).filter(|at| *at > now) {
            format!("is out of usage until {until}")
        } else {
            "is the account that failed".to_owned()
        };
        format!("{} {what}", account.label())
    }
}

/// The highest-priority eligible account of `provider`, other than `skip`.
pub fn first_eligible(
    pool: &Pool<'_>,
    provider: &str,
    skip: Option<&AllowedAccount>,
    now: u64,
) -> Option<AllowedAccount> {
    pool.allowlist
        .ordered()
        .into_iter()
        .filter(|a| a.provider == provider)
        .filter(|a| skip.is_none_or(|s| !s.same(a)))
        .find(|a| pool.eligible(a, now))
        .cloned()
}

/// What to do about the account after a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountPlan {
    /// Retry on the same account (the caller backs off and counts it).
    Same,
    Switch(AllowedAccount),
    /// Nothing is eligible before `until`; retry then.
    Wait {
        until: u64,
    },
    Block(String),
}

/// The account action for `failure` on `failed`, after [`record_failure`].
/// Configuration and policy failures never switch accounts; a shared quota
/// never cycles among the accounts sharing it.
pub fn plan(
    failure: Failure,
    failed: &AllowedAccount,
    pool: &Pool<'_>,
    resets_at: Option<u64>,
    now: u64,
) -> AccountPlan {
    match failure {
        Failure::Temporary | Failure::Unknown => AccountPlan::Same,
        Failure::Configuration | Failure::Policy => AccountPlan::Block(format!(
            "The provider refused the work ({}). Recovery never switches accounts for this; fix the request or the configuration, then unblock the task.",
            failure.as_str().replace('_', " ")
        )),
        Failure::SharedQuota => match resets_at.filter(|at| *at > now) {
            Some(until) => AccountPlan::Wait { until },
            None => AccountPlan::Block(format!(
                "The shared quota of {} is spent and its reset time is unknown. Recovery does not cycle among accounts that share a quota.",
                failed.label()
            )),
        },
        Failure::ProviderUnavailable => {
            let fallback = pool.allowlist.provider_fallback.then(|| {
                pool.allowlist
                    .ordered()
                    .into_iter()
                    .filter(|a| a.provider != failed.provider)
                    .find(|a| pool.eligible(a, now))
                    .cloned()
            });
            match fallback.flatten() {
                Some(next) => AccountPlan::Switch(next),
                None => AccountPlan::Block(format!(
                    "{} is unavailable and no provider fallback is permitted or eligible.",
                    failed.provider
                )),
            }
        }
        Failure::UsageExhausted | Failure::Auth => {
            if let Some(next) = first_eligible(pool, &failed.provider, Some(failed), now) {
                return AccountPlan::Switch(next);
            }
            let same: Vec<_> = pool
                .allowlist
                .ordered()
                .into_iter()
                .filter(|a| a.provider == failed.provider)
                .cloned()
                .chain(
                    (!pool.allowlist.accounts.iter().any(|a| a.same(failed)))
                        .then(|| failed.clone()),
                )
                .collect();
            let reset = resets_at.filter(|at| *at > now);
            let wait = same
                .iter()
                .filter(|a| {
                    let health = pool.health.get(&a.provider, &a.account_id);
                    (pool.exists)(a) && !health.unhealthy
                })
                .filter_map(|a| {
                    let health = pool.health.get(&a.provider, &a.account_id);
                    // Only a known reset is worth waiting for.
                    if a.same(failed) {
                        reset
                    } else {
                        health
                            .cooldown_until
                            .filter(|at| *at > now && health.reset_known)
                    }
                })
                .min();
            match wait {
                Some(until) => AccountPlan::Wait { until },
                None => {
                    let reasons: Vec<String> = same.iter().map(|a| pool.why_not(a, now)).collect();
                    AccountPlan::Block(format!(
                        "No eligible account: {}. Sign in again, wait for a reset or allow another account, then unblock the task.",
                        reasons.join("; ")
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "eligibility_tests.rs"]
mod tests;
