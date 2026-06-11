//! Per-device credit ledger.
//!
//! Identity is an opaque `device_id` (UUID generated client-side, stored in
//! `localStorage` / `chrome.storage.local`). New devices get
//! `FREE_TIER_CREDITS` on first contact; subsequent credits come from Stripe
//! Checkout sessions.
//!
//! v1 storage is an in-process `HashMap` behind a `Mutex`. **Balances are lost
//! on Fly machine restart.** Mitigations:
//! - Restarts are rare and intentional (manual `task fly:deploy`).
//! - Worst case: a few users lose unclaimed credits; we refund on email.
//! - When daily volume justifies it, swap for SQLite on a Fly volume.
//!
//! Decrement semantics: a credit is **reserved at job creation**, not at job
//! completion. `Failed` / `Cancelled` terminal states refund the reservation
//! via [`refund`]. This prevents the race where a user with balance=1 fires
//! 5 parallel jobs and lands at balance=-4 if we decremented only on success.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

pub type CreditStore = Arc<Mutex<HashMap<String, i32>>>;

/// Number of free credits a brand-new device gets on first contact.
/// 3 is enough for a meaningful evaluation without becoming an abuse vector
/// (worst-case spam attack against the rate limiter yields ~3 free
/// transcriptions × ~$0.10 = $0.30 burned per attacker IP).
pub const FREE_TIER_CREDITS: i32 = 3;

pub fn new_store() -> CreditStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Returns the device's current balance, initialising to [`FREE_TIER_CREDITS`]
/// if this is the first time we've seen the id. Pure read after the first call.
pub async fn balance(store: &CreditStore, device_id: &str) -> i32 {
    let mut s = store.lock().await;
    *s.entry(device_id.to_string()).or_insert_with(|| {
        info!(
            "New device {} — seeded with {} free credits",
            short_id(device_id),
            FREE_TIER_CREDITS
        );
        FREE_TIER_CREDITS
    })
}

/// Atomically decrement a device's balance by 1 and return the new balance.
/// Returns `Err(())` if balance is already 0 (or somehow negative). Initialises
/// the device to [`FREE_TIER_CREDITS`] if unseen (so the very first request is
/// always allowed for a new device).
pub async fn reserve(store: &CreditStore, device_id: &str) -> Result<i32, ()> {
    let mut s = store.lock().await;
    let bal = s.entry(device_id.to_string()).or_insert_with(|| {
        info!(
            "New device {} — seeded with {} free credits",
            short_id(device_id),
            FREE_TIER_CREDITS
        );
        FREE_TIER_CREDITS
    });
    if *bal <= 0 {
        return Err(());
    }
    *bal -= 1;
    info!(
        "Reserved 1 credit for device {} — balance now {}",
        short_id(device_id),
        *bal
    );
    Ok(*bal)
}

/// Refund a reservation. Call when a job ends in `Failed` or `Cancelled`.
/// Never goes above the cap implied by [`add`] history — refunds *can* push
/// balance above what the user purchased if multiple jobs fail concurrently,
/// but that's intentional UX (we'd rather over-refund than under-refund).
pub async fn refund(store: &CreditStore, device_id: &str) {
    let mut s = store.lock().await;
    let bal = s.entry(device_id.to_string()).or_insert(0);
    *bal += 1;
    info!(
        "Refunded 1 credit to device {} — balance now {}",
        short_id(device_id),
        *bal
    );
}

/// Add credits to a device (called by the Stripe webhook on successful
/// `checkout.session.completed`). Creates the device with the added amount
/// if previously unseen.
pub async fn add(store: &CreditStore, device_id: &str, amount: i32) -> i32 {
    let mut s = store.lock().await;
    let bal = s.entry(device_id.to_string()).or_insert(0);
    *bal += amount;
    info!(
        "Added {} credits to device {} — balance now {}",
        amount,
        short_id(device_id),
        *bal
    );
    *bal
}

/// Lightweight `device_id` validation — alphanumeric, dashes, max 128 chars.
/// Generous on purpose: clients generate UUIDs but we don't want to break if
/// someone passes a longer cryptographic random string in future.
pub fn is_valid_device_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Short hash for logs so we don't leak the full device id into Fly logs.
fn short_id(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        format!("{}…", &s[..8])
    }
}
