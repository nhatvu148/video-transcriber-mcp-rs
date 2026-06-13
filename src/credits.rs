//! Per-device credit ledger.
//!
//! Identity is an opaque `device_id` (UUID generated client-side, stored in
//! `localStorage` / `chrome.storage.local`). New devices get
//! `FREE_TIER_CREDITS` on first contact; subsequent credits come from Stripe
//! Checkout sessions.
//!
//! ## Persistence
//!
//! Backed by an in-process `HashMap<device_id, balance>` snapshotted to a
//! JSON file after every write. The file path is the env var
//! `CREDITS_DB_PATH` (default `./credits.json`). For Fly deployments point it
//! at a mounted volume (e.g. `/data/credits.json`) so balances survive
//! `auto_stop_machines` restarts and redeploys.
//!
//! Why JSON-on-disk instead of SQLite:
//! - Expected v1 scale: ≤ 10k devices, ≤ a few writes per second.
//! - A full snapshot fits in memory and serialises in milliseconds at this
//!   scale.
//! - One file + atomic rename = zero corruption risk vs. SQLite's WAL
//!   complexity for a single-machine deployment.
//! - When sustained write rate exceeds ~10 writes/sec or balances cross
//!   ~100k devices, swap for SQLite (`rusqlite` + WAL on the same volume).
//!
//! Atomicity: writes happen under the same `Mutex` lock as the in-memory
//! mutation, then `write` + `rename` to a temp file. The rename is atomic on
//! POSIX (Fly's underlying filesystem). If the write itself fails we log and
//! keep going — the in-memory state is authoritative until the next
//! successful write; only a machine crash *during* the unflushed window
//! loses data.
//!
//! Decrement semantics: a credit is **reserved at job creation**, not at job
//! completion. `Failed` / `Cancelled` terminal states refund the reservation
//! via [`refund`]. This prevents the race where a user with balance=1 fires
//! 5 parallel jobs and lands at balance=-4 if we decremented only on success.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Inner state held under the lock. Carrying the path here means every
/// mutator function can persist without threading it through every call site.
pub struct CreditState {
    balances: HashMap<String, i32>,
    path: PathBuf,
}

pub type CreditStore = Arc<Mutex<CreditState>>;

/// Number of free credits a brand-new device gets on first contact.
/// 3 is enough for a meaningful evaluation without becoming an abuse vector
/// (worst-case spam attack against the rate limiter yields ~3 free
/// transcriptions × ~$0.10 = $0.30 burned per attacker IP).
pub const FREE_TIER_CREDITS: i32 = 3;

const DEFAULT_DB_PATH: &str = "./credits.json";

/// Build a fresh store, hydrating from the persisted snapshot if it exists.
/// Path is read from the `CREDITS_DB_PATH` env var (default
/// [`DEFAULT_DB_PATH`]). Missing or unreadable file = empty store + warning;
/// we don't fail the boot because a brand-new volume is the common case.
pub fn new_store() -> CreditStore {
    let path = std::env::var("CREDITS_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB_PATH));

    let balances = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<HashMap<String, i32>>(&s) {
            Ok(map) => {
                info!(
                    "Credits: loaded {} device balances from {}",
                    map.len(),
                    path.display()
                );
                map
            }
            Err(e) => {
                warn!(
                    "Credits: {} is corrupt ({}), starting empty — back up the file before any write",
                    path.display(),
                    e
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(
                "Credits: no existing snapshot at {} — starting fresh",
                path.display()
            );
            HashMap::new()
        }
        Err(e) => {
            warn!(
                "Credits: could not read {} ({}) — starting empty",
                path.display(),
                e
            );
            HashMap::new()
        }
    };

    Arc::new(Mutex::new(CreditState { balances, path }))
}

/// Snapshot the current in-memory map to disk. Atomic via temp-file + rename.
/// Caller must already hold the Mutex (we take `&CreditState`).
fn persist(state: &CreditState) {
    let json = match serde_json::to_string(&state.balances) {
        Ok(s) => s,
        Err(e) => {
            error!("Credits: serialise failed ({}); skipping persist", e);
            return;
        }
    };

    let mut tmp = state.path.clone();
    let ext = format!(
        "{}.tmp",
        tmp.extension().and_then(|s| s.to_str()).unwrap_or("json")
    );
    tmp.set_extension(ext);

    if let Err(e) = std::fs::write(&tmp, json) {
        error!(
            "Credits: write to {} failed ({}); in-memory state is still authoritative",
            tmp.display(),
            e
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &state.path) {
        error!(
            "Credits: rename {} → {} failed ({})",
            tmp.display(),
            state.path.display(),
            e
        );
    }
}

/// Returns the device's current balance, initialising to [`FREE_TIER_CREDITS`]
/// if this is the first time we've seen the id. Persists if a new device was
/// just created.
pub async fn balance(store: &CreditStore, device_id: &str) -> i32 {
    let mut s = store.lock().await;
    let was_new = !s.balances.contains_key(device_id);
    let bal = *s.balances.entry(device_id.to_string()).or_insert_with(|| {
        info!(
            "New device {} — seeded with {} free credits",
            short_id(device_id),
            FREE_TIER_CREDITS
        );
        FREE_TIER_CREDITS
    });
    if was_new {
        persist(&s);
    }
    bal
}

/// Atomically decrement a device's balance by 1 and return the new balance.
/// Returns `Err(())` if balance is already 0 (or somehow negative). Initialises
/// the device to [`FREE_TIER_CREDITS`] if unseen (so the very first request is
/// always allowed for a new device).
pub async fn reserve(store: &CreditStore, device_id: &str) -> Result<i32, ()> {
    let mut s = store.lock().await;
    {
        let bal = s.balances.entry(device_id.to_string()).or_insert_with(|| {
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
    }
    let new = *s.balances.get(device_id).unwrap_or(&0);
    persist(&s);
    Ok(new)
}

/// Refund a reservation. Call when a job ends in `Failed` or `Cancelled`.
pub async fn refund(store: &CreditStore, device_id: &str) {
    let mut s = store.lock().await;
    {
        let bal = s.balances.entry(device_id.to_string()).or_insert(0);
        *bal += 1;
        info!(
            "Refunded 1 credit to device {} — balance now {}",
            short_id(device_id),
            *bal
        );
    }
    persist(&s);
}

/// Add credits to a device (called by the Stripe webhook on successful
/// `checkout.session.completed`).
pub async fn add(store: &CreditStore, device_id: &str, amount: i32) -> i32 {
    let mut s = store.lock().await;
    let new = {
        let bal = s.balances.entry(device_id.to_string()).or_insert(0);
        *bal += amount;
        info!(
            "Added {} credits to device {} — balance now {}",
            amount,
            short_id(device_id),
            *bal
        );
        *bal
    };
    persist(&s);
    new
}

/// Lightweight `device_id` validation — alphanumeric, dashes, max 128 chars.
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
