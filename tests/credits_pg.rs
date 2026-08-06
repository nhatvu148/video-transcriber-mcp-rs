//! Postgres-backed credit ledger tests — the money path.
//!
//! The unit tests in `src/credits.rs` all construct `CreditStore::File`, so
//! until now every `CreditStore::Db` branch (and therefore every sqlx call
//! site) shipped unverified. A sqlx major bump can change type decoding, TLS
//! negotiation or transaction semantics without producing a single compile
//! error, and `new_store()` *aborts at boot* when `DATABASE_URL` is set and
//! the connection fails — so a regression here is a hard production outage,
//! not a degraded mode.
//!
//! These run only when `DATABASE_URL` is set, and skip cleanly otherwise so a
//! plain `cargo test` still works with no database. CI provides a throwaway
//! Postgres service container.
//!
//! **Point `DATABASE_URL` at a scratch database.** These tests write real
//! rows, and `new_store()` will import a local `credits.json` into an empty
//! table on first connect.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use video_transcriber_mcp::credits::{
    self, ClaimOutcome, CreditStore, FREE_TIER_CREDITS, account_key,
};

/// Returns a connected store, or `None` when no database is configured.
async fn store() -> Option<CreditStore> {
    let url = std::env::var("DATABASE_URL").ok()?;
    if url.trim().is_empty() {
        return None;
    }
    let store = credits::new_store().await;
    assert!(
        matches!(store, CreditStore::Db(_)),
        "DATABASE_URL is set but new_store() did not choose the Postgres backend"
    );
    Some(store)
}

/// Skip the test body when there's no database, printing why.
macro_rules! store_or_skip {
    () => {
        match store().await {
            Some(s) => s,
            None => {
                eprintln!("skipping: DATABASE_URL not set");
                return;
            }
        }
    };
}

/// A ledger id no other test (or run) will collide with, so the suite is
/// parallel-safe against a shared database.
fn unique_id(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("test:{tag}:{nanos}:{}:{n}", std::process::id())
}

#[tokio::test]
async fn connects_and_seeds_the_free_tier_on_first_read() {
    let store = store_or_skip!();
    let id = unique_id("seed");
    assert_eq!(credits::balance(&store, &id).await, FREE_TIER_CREDITS);
    // Seeding is a first-read effect; reading again must not top it back up.
    assert_eq!(credits::balance(&store, &id).await, FREE_TIER_CREDITS);
}

#[tokio::test]
async fn reserve_decrements_until_exhausted_then_fails() {
    let store = store_or_skip!();
    let id = unique_id("reserve");

    for expected in (0..FREE_TIER_CREDITS).rev() {
        assert_eq!(
            credits::reserve(&store, &id).await,
            Ok(expected),
            "reserve should return the remaining balance"
        );
    }
    assert_eq!(credits::balance(&store, &id).await, 0);
    assert_eq!(
        credits::reserve(&store, &id).await,
        Err(()),
        "reserve must fail once the balance hits zero"
    );
    assert_eq!(credits::balance(&store, &id).await, 0, "no negative balances");
}

#[tokio::test]
async fn refund_restores_a_reserved_credit() {
    let store = store_or_skip!();
    let id = unique_id("refund");

    credits::reserve(&store, &id).await.expect("first reserve");
    let after_reserve = credits::balance(&store, &id).await;
    credits::refund(&store, &id).await;
    assert_eq!(credits::balance(&store, &id).await, after_reserve + 1);
}

#[tokio::test]
async fn add_credits_the_stripe_webhook_amount() {
    let store = store_or_skip!();
    let id = unique_id("add");

    let before = credits::balance(&store, &id).await;
    let after = credits::add(&store, &id, 50).await;
    assert_eq!(after, before + 50, "add must return the new balance");
    assert_eq!(credits::balance(&store, &id).await, before + 50);
}

#[tokio::test]
async fn claim_seeds_then_is_idempotent() {
    let store = store_or_skip!();
    let user = unique_id("claim");

    assert_eq!(
        credits::claim_account(&store, &user, None).await,
        ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS }
    );
    assert_eq!(
        credits::claim_account(&store, &user, None).await,
        ClaimOutcome::AlreadyClaimed { balance: FREE_TIER_CREDITS },
        "a second claim must not re-seed credits"
    );
    assert_eq!(
        credits::balance(&store, &account_key(&user)).await,
        FREE_TIER_CREDITS
    );
}

#[tokio::test]
async fn claim_migrates_a_device_balance_and_drains_the_device() {
    let store = store_or_skip!();
    let user = unique_id("migrate-user");
    let device = unique_id("migrate-device");

    credits::add(&store, &device, 34).await;
    let device_total = credits::balance(&store, &device).await;

    assert_eq!(
        credits::claim_account(&store, &user, Some(&device)).await,
        ClaimOutcome::Migrated { from_device: device_total, balance: device_total }
    );
    assert_eq!(credits::balance(&store, &account_key(&user)).await, device_total);
    assert_eq!(
        credits::balance(&store, &device).await,
        0,
        "the device balance must be drained so credits can't be spent twice"
    );
}

/// The test that actually earns its keep: concurrent reserves against a
/// single balance. If a sqlx upgrade changed transaction or isolation
/// behaviour, this oversells and every other test here still passes.
#[tokio::test]
async fn concurrent_reserves_never_oversell() {
    let store = store_or_skip!();
    let id = unique_id("race");

    let funded = 25;
    credits::add(&store, &id, funded).await;
    let total = credits::balance(&store, &id).await;

    // Twice as many contenders as there are credits.
    let attempts = (total * 2) as usize;
    let store = std::sync::Arc::new(store);
    let mut tasks = Vec::with_capacity(attempts);
    for _ in 0..attempts {
        let store = store.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(
            async move { credits::reserve(&store, &id).await },
        ));
    }

    let mut granted = 0;
    for task in tasks {
        if task.await.expect("reserve task panicked").is_ok() {
            granted += 1;
        }
    }

    assert_eq!(
        granted, total,
        "exactly {total} reserves should succeed, not {granted}"
    );
    assert_eq!(
        credits::balance(&store, &id).await,
        0,
        "balance must land exactly on zero"
    );
}
