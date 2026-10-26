//! A Mono's own chat recovers from account failures under its allowlist;
//! ordinary conversations and Monos without one keep today's behaviour.
use super::*;
use monocode_core::accounts::eligibility::{Allowlist, Scope, save_allowlist};
use monocode_core::accounts::{Account, Book};
use std::path::Path;

const PROJECT: &str = "/fixture";

fn database() -> Database {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .save_provider_accounts(&Book {
            accounts: vec![Account {
                id: "account-b".into(),
                provider: "codex".into(),
                label: "B".into(),
            }],
            ..Book::default()
        })
        .unwrap();
    Arc::new(std::sync::Mutex::new(store))
}

fn allow(store: &Database, scope: Scope<'_>, groups: [Option<&str>; 2]) {
    let mut list = Allowlist::only("codex", "default");
    list.accounts.push(AllowedAccount {
        priority: 1,
        ..AllowedAccount::new("codex", "account-b")
    });
    for (account, group) in list.accounts.iter_mut().zip(groups) {
        account.quota_group = group.map(str::to_owned);
    }
    save_allowlist(&locked(store).unwrap(), scope, &list).unwrap();
}

/// A conversation in `PROJECT` with history; a Mono's chat when `mono`.
fn chat(
    store: &Database,
    mono: bool,
    cx: &mut gpui::TestAppContext,
) -> (Entity<Conversation>, std::rc::Rc<std::cell::Cell<u32>>) {
    let summary = locked(store).unwrap().create("codex", PROJECT).unwrap();
    let view = cx.new(|cx| Conversation::new(summary, store.clone(), cx));
    cx.run_until_parked();
    view.update(cx, |v, _| {
        v.summary.has_messages = true;
        if mono {
            v.settings.mono = Some(monocode_core::agent::MonoContext {
                mono: monocode_core::mono::Mono {
                    id: "mono-1".into(),
                    ..Default::default()
                },
                root: PROJECT.into(),
                executable: "/fixture/binary".into(),
            });
        }
    });
    let resumed = std::rc::Rc::new(std::cell::Cell::new(0));
    cx.update(|cx| {
        let count = resumed.clone();
        cx.subscribe(&view, move |_, _: &UsageResumed, _| {
            count.set(count.get() + 1)
        })
        .detach();
    });
    (view, resumed)
}

fn fail(view: &Entity<Conversation>, status: &str, cx: &mut gpui::TestAppContext) {
    view.update(cx, |v, cx| {
        v.status = status.into();
        v.detect_usage_limit(cx);
    });
    cx.run_until_parked();
}

fn available(store: &Database, account: &str) -> bool {
    eligibility::health(&locked(store).unwrap())
        .unwrap()
        .get("codex", account)
        .available(monocode_core::time::now_ms())
}

#[gpui::test]
fn a_mono_chat_moves_to_its_next_allowed_account_after_a_usage_limit(
    cx: &mut gpui::TestAppContext,
) {
    let store = database();
    allow(&store, Scope::Mono("mono-1"), [None, None]);
    let (view, resumed) = chat(&store, true, cx);
    fail(
        &view,
        "Usage limit reached: Fixture subscription exhausted",
        cx,
    );
    view.read_with(cx, |v, _| {
        assert_eq!(v.settings.account.id, "account-b");
        assert!(v.settings.usage_limit.is_none(), "the turn resumed");
    });
    assert_eq!(resumed.get(), 1);
    assert!(!available(&store, "default"), "the spent account rests");
    // The next limit has nowhere to go: account-b rests too and the chat
    // never cycles back to the account that is still cooling down.
    fail(&view, "Usage limit reached again", cx);
    view.read_with(cx, |v, _| {
        assert_eq!(v.settings.account.id, "account-b");
        assert!(v.settings.usage_limit.is_some());
        assert!(v.status.starts_with("No eligible account"), "{}", v.status);
    });
    assert_eq!(resumed.get(), 1);
}

#[gpui::test]
fn a_mono_chat_never_switches_on_a_shared_quota_or_a_configuration_error(
    cx: &mut gpui::TestAppContext,
) {
    let store = database();
    allow(&store, Scope::Mono("mono-1"), [Some("org"), Some("org")]);
    let (view, resumed) = chat(&store, true, cx);
    fail(
        &view,
        "Your organization's usage limit has been reached",
        cx,
    );
    view.read_with(cx, |v, _| {
        assert_eq!(v.settings.account.id, "default");
        assert!(v.status.contains("shared quota"), "{}", v.status);
    });
    assert!(!available(&store, "default") && !available(&store, "account-b"));
    let store = database();
    allow(&store, Scope::Mono("mono-1"), [None, None]);
    let (view, _) = chat(&store, true, cx);
    fail(&view, "model_not_found: gpt-9 does not exist", cx);
    view.read_with(cx, |v, _| assert_eq!(v.settings.account.id, "default"));
    assert!(available(&store, "default"), "no account is blamed");
    assert_eq!(resumed.get(), 0);
}

#[gpui::test]
fn ordinary_chats_and_monos_without_an_allowlist_keep_their_account(cx: &mut gpui::TestAppContext) {
    // An ordinary conversation is untouched even with a project allowlist.
    let store = database();
    allow(&store, Scope::Project(PROJECT), [None, None]);
    let (view, _) = chat(&store, false, cx);
    fail(&view, "Usage limit reached", cx);
    view.read_with(cx, |v, _| {
        assert_eq!(v.settings.account.id, "default");
        assert!(v.settings.usage_limit.is_some());
    });
    assert!(available(&store, "default"));
    // A Mono without an allowlist waits on its own account as before.
    let store = database();
    let (view, resumed) = chat(&store, true, cx);
    fail(&view, "Usage limit reached", cx);
    view.read_with(cx, |v, _| {
        assert_eq!(v.settings.account.id, "default");
        assert!(v.settings.usage_limit.is_some());
    });
    assert_eq!(resumed.get(), 0);
    assert!(available(&store, "default"));
}
