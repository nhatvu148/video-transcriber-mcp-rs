//! Credit ledger keyed by an opaque identity string.
//!
//! ## Identity
//!
//! Two kinds of identity key share this ledger:
//! - **Account keys** (`user:<supabase-uuid>`) — the primary identity since
//!   the email-auth migration. Granted [`FREE_TIER_CREDITS`] once per account.
//!   Because creating a Supabase account requires Google/email verification,
//!   farming free credits across many accounts is no longer cheap — this is
//!   the abuse-resistant free tier.
//! - **Device keys** (raw `device_id` UUIDs) — the legacy anonymous identity.
//!   Still honored for backward compatibility (older clients, the extension
//!   before it gains auth), and as the *source* of a one-time migration: on a
//!   user's first sign-in, [`claim_account`] transfers their device balance to
//!   their account so purchased credits aren't lost.
//!
//! The two namespaces never collide because account keys carry the `user:`
//! prefix while device keys are bare UUIDs. Both are just `String`s in the
//! same map; the ledger itself is identity-agnostic.
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

/// Build the ledger key for an authenticated account from a Supabase user id.
/// The `user:` prefix namespaces accounts away from legacy device keys.
pub fn account_key(user_id: &str) -> String {
    format!("user:{user_id}")
}

/// Outcome of [`claim_account`], surfaced so the API/UI can tell the user what
/// happened ("migrated 34 credits" vs "welcome, here are 3 free credits").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Account already existed — nothing changed. `balance` is the current value.
    AlreadyClaimed { balance: i32 },
    /// Device balance was transferred into a brand-new account.
    Migrated { from_device: i32, balance: i32 },
    /// Brand-new account with no device history — seeded with the free tier.
    Seeded { balance: i32 },
}

/// One-time account bootstrap, run on a user's first authenticated request.
///
/// - If the account already has a ledger entry → no-op (`AlreadyClaimed`).
/// - Else if `device_key` is provided and that device has a balance → transfer
///   the whole balance to the account and zero the device (`Migrated`). No free
///   tier is added on top: an anonymous user who used the app already consumed
///   their device's free grant, so granting again would let one human double-dip.
/// - Else → seed the account with [`FREE_TIER_CREDITS`] (`Seeded`).
///
/// Idempotent: calling it repeatedly after the first time returns
/// `AlreadyClaimed` without mutating anything.
pub async fn claim_account(
    store: &CreditStore,
    user_id: &str,
    device_key: Option<&str>,
) -> ClaimOutcome {
    let key = account_key(user_id);
    let mut s = store.lock().await;

    if let Some(&existing) = s.balances.get(&key) {
        return ClaimOutcome::AlreadyClaimed { balance: existing };
    }

    // Account is new. Decide between migrate-from-device and fresh-free-tier.
    let migrate_amount = device_key
        .and_then(|d| s.balances.get(d).copied())
        .filter(|&bal| bal > 0);

    let outcome = if let Some(amount) = migrate_amount {
        let device = device_key.expect("migrate_amount implies device_key");
        s.balances.insert(key.clone(), amount);
        // Zero the device so the credits can't be claimed twice (e.g. a second
        // account claiming the same device). We keep the key at 0 rather than
        // removing it so a stale anonymous client doesn't silently re-seed.
        s.balances.insert(device.to_string(), 0);
        info!(
            "Claimed account {} — migrated {} credits from device {}",
            short_id(user_id),
            amount,
            short_id(device)
        );
        ClaimOutcome::Migrated {
            from_device: amount,
            balance: amount,
        }
    } else {
        s.balances.insert(key.clone(), FREE_TIER_CREDITS);
        info!(
            "Claimed account {} — seeded with {} free credits",
            short_id(user_id),
            FREE_TIER_CREDITS
        );
        ClaimOutcome::Seeded {
            balance: FREE_TIER_CREDITS,
        }
    };
    persist(&s);
    outcome
}

/// Returns the identity's current balance. For account keys (`user:…`) the
/// entry is expected to already exist via [`claim_account`]; if it somehow
/// doesn't, we seed the free tier as a safety net. For legacy device keys this
/// preserves the original seed-on-read behavior.
pub async fn balance(store: &CreditStore, id: &str) -> i32 {
    let mut s = store.lock().await;
    let was_new = !s.balances.contains_key(id);
    let bal = *s.balances.entry(id.to_string()).or_insert_with(|| {
        info!(
            "New identity {} — seeded with {} free credits",
            short_id(id),
            FREE_TIER_CREDITS
        );
        FREE_TIER_CREDITS
    });
    if was_new {
        persist(&s);
    }
    bal
}

/// Atomically decrement an identity's balance by 1 and return the new balance.
/// Returns `Err(())` if balance is already 0 (or somehow negative). Seeds the
/// free tier if unseen (so a first request from a never-claimed identity is
/// still allowed).
pub async fn reserve(store: &CreditStore, id: &str) -> Result<i32, ()> {
    let mut s = store.lock().await;
    {
        let bal = s.balances.entry(id.to_string()).or_insert_with(|| {
            info!(
                "New identity {} — seeded with {} free credits",
                short_id(id),
                FREE_TIER_CREDITS
            );
            FREE_TIER_CREDITS
        });
        if *bal <= 0 {
            return Err(());
        }
        *bal -= 1;
        info!(
            "Reserved 1 credit for {} — balance now {}",
            short_id(id),
            *bal
        );
    }
    let new = *s.balances.get(id).unwrap_or(&0);
    persist(&s);
    Ok(new)
}

/// Refund a reservation. Call when a job ends in `Failed` or `Cancelled`.
pub async fn refund(store: &CreditStore, id: &str) {
    let mut s = store.lock().await;
    {
        let bal = s.balances.entry(id.to_string()).or_insert(0);
        *bal += 1;
        info!("Refunded 1 credit to {} — balance now {}", short_id(id), *bal);
    }
    persist(&s);
}

/// Add credits to an identity (called by the Stripe webhook on successful
/// `checkout.session.completed`).
pub async fn add(store: &CreditStore, id: &str, amount: i32) -> i32 {
    let mut s = store.lock().await;
    let new = {
        let bal = s.balances.entry(id.to_string()).or_insert(0);
        *bal += amount;
        info!(
            "Added {} credits to {} — balance now {}",
            amount,
            short_id(id),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an in-memory store pointed at a temp file so persist() is a no-op
    /// we don't care about (writes succeed to /tmp, nothing asserts on them).
    fn test_store(initial: &[(&str, i32)]) -> CreditStore {
        let mut balances = HashMap::new();
        for (k, v) in initial {
            balances.insert(k.to_string(), *v);
        }
        let mut path = std::env::temp_dir();
        // Unique-ish filename without Date/random (forbidden) — use a counter
        // via the initial contents length + a fixed prefix. Collisions are
        // harmless since each test writes its own snapshot.
        path.push(format!("credits-test-{}.json", balances.len()));
        Arc::new(Mutex::new(CreditState { balances, path }))
    }

    #[tokio::test]
    async fn claim_seeds_free_tier_for_brand_new_account() {
        let store = test_store(&[]);
        let outcome = claim_account(&store, "user-uuid-1", None).await;
        assert_eq!(outcome, ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS });
        assert_eq!(balance(&store, &account_key("user-uuid-1")).await, FREE_TIER_CREDITS);
    }

    #[tokio::test]
    async fn claim_migrates_device_balance() {
        let store = test_store(&[("device-abc", 34)]);
        let outcome = claim_account(&store, "user-uuid-2", Some("device-abc")).await;
        assert_eq!(
            outcome,
            ClaimOutcome::Migrated { from_device: 34, balance: 34 }
        );
        // Account got the credits...
        assert_eq!(balance(&store, &account_key("user-uuid-2")).await, 34);
        // ...and the device was zeroed so it can't be claimed again.
        assert_eq!(balance(&store, "device-abc").await, 0);
    }

    #[tokio::test]
    async fn claim_is_idempotent() {
        let store = test_store(&[]);
        let first = claim_account(&store, "user-uuid-3", None).await;
        assert_eq!(first, ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS });
        // Second claim with a juicy device balance must NOT stack — account
        // already exists, so nothing changes.
        let second = claim_account(&store, "user-uuid-3", Some("device-xyz")).await;
        assert_eq!(second, ClaimOutcome::AlreadyClaimed { balance: FREE_TIER_CREDITS });
    }

    #[tokio::test]
    async fn claim_with_empty_device_seeds_free_tier() {
        // Device key points at a zero (or missing) balance → no migration.
        let store = test_store(&[("device-empty", 0)]);
        let outcome = claim_account(&store, "user-uuid-4", Some("device-empty")).await;
        assert_eq!(outcome, ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS });
    }

    #[tokio::test]
    async fn reserve_and_refund_roundtrip() {
        let store = test_store(&[]);
        claim_account(&store, "user-uuid-5", None).await;
        let key = account_key("user-uuid-5");
        let after_reserve = reserve(&store, &key).await.unwrap();
        assert_eq!(after_reserve, FREE_TIER_CREDITS - 1);
        refund(&store, &key).await;
        assert_eq!(balance(&store, &key).await, FREE_TIER_CREDITS);
    }

    #[tokio::test]
    async fn reserve_fails_at_zero() {
        let store = test_store(&[("user:broke", 0)]);
        assert!(reserve(&store, "user:broke").await.is_err());
    }
}
