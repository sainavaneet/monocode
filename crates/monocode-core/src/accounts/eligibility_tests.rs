//! Allowlists, cooldown and health, and the account each failure leads to.
use super::*;
use crate::store::Store;
use crate::usage::Failure;
use std::path::Path;

const NOW: u64 = 1_000_000;

fn store() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}

fn list(entries: &[(&str, i64, Option<&str>)]) -> Allowlist {
    Allowlist {
        accounts: entries
            .iter()
            .map(|(id, priority, group)| AllowedAccount {
                provider: "codex".into(),
                account_id: (*id).into(),
                priority: *priority,
                quota_group: group.map(str::to_owned),
            })
            .collect(),
        provider_fallback: false,
    }
}

fn pool<'a>(allowlist: &'a Allowlist, health: &'a HealthBook) -> Pool<'a> {
    Pool {
        allowlist,
        health,
        exists: &|_| true,
        exhausted_until: &|_| None,
    }
}

#[test]
fn allowlists_and_health_live_in_native_settings_with_account_ids_only() {
    let store = store();
    assert_eq!(
        allowlist_for(&store, Some("mono-1"), "/work").unwrap(),
        None
    );
    let project = list(&[("default", 0, None), ("account-b", 1, None)]);
    save_allowlist(&store, Scope::Project("/work/"), &project).unwrap();
    // A project path is keyed the way account selections are.
    assert_eq!(
        allowlist_for(&store, Some("mono-1"), "/work").unwrap(),
        Some(project.clone())
    );
    let mono = list(&[("account-c", 0, None)]);
    save_allowlist(&store, Scope::Mono("mono-1"), &mono).unwrap();
    // The Mono's own list wins over the project's.
    assert_eq!(
        allowlist_for(&store, Some("mono-1"), "/work").unwrap(),
        Some(mono)
    );
    assert_eq!(
        allowlist_for(&store, Some("mono-2"), "/work").unwrap(),
        Some(project)
    );
    // Ids are validated; nothing but ids, priorities and groups is stored.
    let bad = list(&[("../escape", 0, None)]);
    assert!(save_allowlist(&store, Scope::Mono("mono-1"), &bad).is_err());
    cool_down(
        &store,
        "codex",
        "default",
        NOW + 50,
        Failure::UsageExhausted,
        NOW,
    )
    .unwrap();
    mark_unhealthy(&store, "codex", "account-b", NOW).unwrap();
    let book = health(&store).unwrap();
    assert!(!book.get("codex", "default").available(NOW + 49));
    assert!(book.get("codex", "default").available(NOW + 50));
    assert!(!book.get("codex", "account-b").available(NOW + 1_000_000));
    mark_healthy(&store, "codex", "account-b", NOW).unwrap();
    assert!(health(&store)
        .unwrap()
        .get("codex", "account-b")
        .available(NOW));
    for key in [
        "native-account-health",
        "native-account-allowlist:mono:mono-1",
    ] {
        let saved = store.setting(key).unwrap().unwrap();
        for word in ["token", "secret", "password", "apiKey", "credential"] {
            assert!(!saved.contains(word), "{key} stores {word}: {saved}");
        }
    }
}

#[test]
fn selection_follows_priority_and_skips_cooled_unhealthy_removed_and_exhausted_accounts() {
    let allowlist = list(&[
        ("account-c", 2, None),
        ("default", 0, None),
        ("account-b", 1, None),
        ("account-d", 3, None),
    ]);
    let mut book = HealthBook::default();
    let choose = |book: &HealthBook| first_eligible(&pool(&allowlist, book), "codex", None, NOW);
    assert_eq!(choose(&book).unwrap().account_id, "default");
    book.set(
        "codex",
        "default",
        Health::cooling(NOW + 10, Failure::UsageExhausted, NOW),
    );
    assert_eq!(choose(&book).unwrap().account_id, "account-b");
    book.set("codex", "account-b", Health::unhealthy(NOW));
    assert_eq!(choose(&book).unwrap().account_id, "account-c");
    // Another provider's accounts are not candidates.
    assert!(first_eligible(&pool(&allowlist, &book), "claude", None, NOW).is_none());
    // Removed accounts and accounts the usage cache shows exhausted are skipped.
    let removed = |a: &AllowedAccount| a.account_id != "account-c";
    let exhausted = |a: &AllowedAccount| (a.account_id == "account-d").then_some(NOW + 99);
    let narrow = Pool {
        allowlist: &allowlist,
        health: &book,
        exists: &removed,
        exhausted_until: &exhausted,
    };
    assert!(first_eligible(&narrow, "codex", None, NOW).is_none());
    // The cooldown expiring makes the first account eligible again.
    assert_eq!(
        first_eligible(&narrow, "codex", None, NOW + 10)
            .unwrap()
            .account_id,
        "default"
    );
}

#[test]
fn every_failure_category_plans_its_account_action() {
    let store = store();
    let allowlist = list(&[
        ("default", 0, Some("org")),
        ("account-b", 1, Some("org")),
        ("account-c", 2, None),
    ]);
    let failed = allowlist.accounts[0].clone();
    let plan_for = |failure: Failure, resets: Option<u64>| {
        record_failure(&store, &allowlist, &failed, failure, resets, NOW).unwrap();
        let book = health(&store).unwrap();
        plan(failure, &failed, &pool(&allowlist, &book), resets, NOW)
    };
    assert_eq!(plan_for(Failure::Temporary, None), AccountPlan::Same);
    assert_eq!(plan_for(Failure::Unknown, None), AccountPlan::Same);
    assert!(matches!(
        plan_for(Failure::Configuration, None),
        AccountPlan::Block(reason) if reason.contains("never switches accounts")
    ));
    assert!(matches!(
        plan_for(Failure::Policy, None),
        AccountPlan::Block(_)
    ));
    assert!(matches!(
        plan_for(Failure::ProviderUnavailable, None),
        AccountPlan::Block(reason) if reason.contains("provider fallback")
    ));
    // None of those touched the account's standing.
    assert!(health(&store)
        .unwrap()
        .get("codex", "default")
        .available(NOW));

    // Exhaustion cools the account down and moves to the next allowed one.
    match plan_for(Failure::UsageExhausted, Some(NOW + 500)) {
        AccountPlan::Switch(next) => assert_eq!(next.account_id, "account-b"),
        other => panic!("{other:?}"),
    }
    let book = health(&store).unwrap();
    assert!(!book.get("codex", "default").available(NOW + 499));
    assert!(book.get("codex", "account-b").available(NOW));

    // A shared quota cools down everyone sharing it and never cycles among
    // them: it waits for the reset (or blocks when the reset is unknown).
    let store = self::store();
    record_failure(
        &store,
        &allowlist,
        &failed,
        Failure::SharedQuota,
        Some(NOW + 700),
        NOW,
    )
    .unwrap();
    let book = health(&store).unwrap();
    assert!(!book.get("codex", "account-b").available(NOW + 1));
    assert!(book.get("codex", "account-c").available(NOW + 1));
    assert_eq!(
        plan(
            Failure::SharedQuota,
            &failed,
            &pool(&allowlist, &book),
            Some(NOW + 700),
            NOW
        ),
        AccountPlan::Wait { until: NOW + 700 }
    );
    assert!(matches!(
        plan(Failure::SharedQuota, &failed, &pool(&allowlist, &book), None, NOW),
        AccountPlan::Block(reason) if reason.contains("shared")
    ));

    // Bad credentials make the account unhealthy until it signs in again.
    let store = self::store();
    let alone = list(&[("default", 0, None)]);
    record_failure(&store, &alone, &failed, Failure::Auth, None, NOW).unwrap();
    let book = health(&store).unwrap();
    assert!(!book.get("codex", "default").available(NOW + 10_000_000));
    assert!(matches!(
        plan(Failure::Auth, &failed, &pool(&alone, &book), None, NOW),
        AccountPlan::Block(reason) if reason.contains("No eligible account") && reason.contains("sign")
    ));
    // Exhaustion with nowhere to go waits for a known reset, else blocks.
    let store = self::store();
    record_failure(
        &store,
        &alone,
        &failed,
        Failure::UsageExhausted,
        Some(NOW + 90),
        NOW,
    )
    .unwrap();
    let book = health(&store).unwrap();
    assert_eq!(
        plan(
            Failure::UsageExhausted,
            &failed,
            &pool(&alone, &book),
            Some(NOW + 90),
            NOW
        ),
        AccountPlan::Wait { until: NOW + 90 }
    );
    assert!(matches!(
        plan(Failure::UsageExhausted, &failed, &pool(&alone, &book), None, NOW),
        AccountPlan::Block(reason) if reason.contains("No eligible account")
    ));

    // A permitted provider fallback moves to another provider's account.
    let mut fallback = list(&[("default", 0, None)]);
    fallback.provider_fallback = true;
    fallback.accounts.push(AllowedAccount {
        provider: "claude".into(),
        account_id: "default".into(),
        priority: 1,
        quota_group: None,
    });
    let book = HealthBook::default();
    match plan(
        Failure::ProviderUnavailable,
        &failed,
        &pool(&fallback, &book),
        None,
        NOW,
    ) {
        AccountPlan::Switch(next) => assert_eq!(next.provider, "claude"),
        other => panic!("{other:?}"),
    }
}
