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
//!   Still honored for backward compatibility and as the *source* of a
//!   one-time migration in [`claim_account`].
//!
//! ## Backend
//!
//! Two interchangeable backends, chosen at startup:
//! - **Postgres** (Supabase) when `DATABASE_URL` is set. The production
//!   choice — money data deserves automatic backups, ACID, and durability
//!   beyond a single Fly volume. Mutations use atomic SQL so concurrent jobs
//!   can't double-spend.
//! - **JSON file** otherwise (the original `credits.json` on disk). Keeps the
//!   open-source engine runnable standalone with zero infra.
//!
//! If `DATABASE_URL` is set but the connection fails at boot, we **abort**
//! rather than silently fall back to the file — diverging money state across
//! backends is worse than a loud startup failure.
//!
//! Decrement semantics: a credit is **reserved at job creation**, not at job
//! completion. `Failed`/`Cancelled` terminal states refund via [`refund`].
//! This prevents the race where a user with balance=1 fires 5 parallel jobs
//! and lands at balance=-4 if we decremented only on success.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Number of free credits a brand-new account gets on first claim.
/// 3 is enough for a meaningful evaluation; account-based identity (verified
/// email/Google) makes multi-account farming uneconomical.
pub const FREE_TIER_CREDITS: i32 = 3;

const DEFAULT_DB_PATH: &str = "./credits.json";

/// Idempotent schema bootstrap — run at startup so the engine works even if
/// the operator didn't run the SQL by hand. Mirrors the documented schema.
const CREATE_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS public.credits (
        id          text PRIMARY KEY,
        balance     integer NOT NULL DEFAULT 0,
        created_at  timestamptz NOT NULL DEFAULT now(),
        updated_at  timestamptz NOT NULL DEFAULT now()
    )
";

/// Backend-agnostic credit store. Operations dispatch on the active backend.
#[derive(Clone)]
pub enum CreditStore {
    /// Postgres (Supabase) — production.
    Db(PgPool),
    /// JSON-on-disk — standalone/fork fallback.
    File(Arc<Mutex<FileState>>),
}

/// File-backend inner state held under the lock.
pub struct FileState {
    balances: HashMap<String, i32>,
    path: PathBuf,
}

/// Build the store. Async because the Postgres pool connects here.
///
/// - `DATABASE_URL` set → connect Postgres, ensure schema, migrate any
///   existing `credits.json` into the table (one-time), return `Db`.
/// - else → load the JSON file, return `File`.
pub async fn new_store() -> CreditStore {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.trim().is_empty() {
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .acquire_timeout(Duration::from_secs(10))
                .connect(&url)
                .await
                .expect(
                    "DATABASE_URL is set but the Postgres connection failed — \
                     refusing to start with divergent money state. Check the \
                     connection string / network and retry.",
                );
            if let Err(e) = sqlx::query(CREATE_TABLE_SQL).execute(&pool).await {
                // Table may already exist with the right shape and the role may
                // lack CREATE — that's fine as long as the table is there. Log
                // and continue; the first real query will surface a hard error.
                warn!("Credits: ensure-table failed ({e}); assuming table exists");
            }
            migrate_file_into_db_if_empty(&pool).await;
            info!("Credits: using Postgres backend");
            return CreditStore::Db(pool);
        }
    }

    // ---- File fallback ----
    let path = std::env::var("CREDITS_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB_PATH));

    let balances = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<HashMap<String, i32>>(&s) {
            Ok(map) => {
                info!(
                    "Credits: loaded {} balances from {} (file backend)",
                    map.len(),
                    path.display()
                );
                map
            }
            Err(e) => {
                warn!(
                    "Credits: {} is corrupt ({}), starting empty — back up before any write",
                    path.display(),
                    e
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("Credits: no snapshot at {} — starting fresh", path.display());
            HashMap::new()
        }
        Err(e) => {
            warn!("Credits: could not read {} ({}) — starting empty", path.display(), e);
            HashMap::new()
        }
    };

    CreditStore::File(Arc::new(Mutex::new(FileState { balances, path })))
}

/// One-time import of `credits.json` into Postgres, only if the table is empty.
/// Lets an existing file-backed deployment switch to Postgres without losing
/// balances. Idempotent: once the table has any row, this is a no-op.
async fn migrate_file_into_db_if_empty(pool: &PgPool) {
    let count: i64 = match sqlx::query_scalar("SELECT COUNT(*) FROM public.credits")
        .fetch_one(pool)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!("Credits: could not count rows for migration check ({e})");
            return;
        }
    };
    if count > 0 {
        return; // table already populated — nothing to migrate
    }

    let path = std::env::var("CREDITS_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB_PATH));
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return; // no file to migrate
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, i32>>(&contents) else {
        warn!("Credits: {} unreadable during DB migration", path.display());
        return;
    };
    if map.is_empty() {
        return;
    }
    let mut migrated = 0;
    for (id, balance) in &map {
        let res = sqlx::query(
            "INSERT INTO public.credits (id, balance) VALUES ($1, $2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(*balance)
        .execute(pool)
        .await;
        match res {
            Ok(_) => migrated += 1,
            Err(e) => warn!("Credits: migrate row {} failed ({e})", short_id(id)),
        }
    }
    info!(
        "Credits: migrated {} balances from {} into Postgres",
        migrated,
        path.display()
    );
}

/// Snapshot the file-backend map to disk. Atomic via temp-file + rename.
/// Caller holds the Mutex.
fn persist(state: &FileState) {
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
        error!("Credits: write to {} failed ({})", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &state.path) {
        error!("Credits: rename {} → {} failed ({})", tmp.display(), state.path.display(), e);
    }
}

/// Build the ledger key for an authenticated account from a Supabase user id.
pub fn account_key(user_id: &str) -> String {
    format!("user:{user_id}")
}

/// Outcome of [`claim_account`], surfaced so the API/UI can tell the user what
/// happened ("migrated 34 credits" vs "welcome, here are 3 free credits").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    AlreadyClaimed { balance: i32 },
    Migrated { from_device: i32, balance: i32 },
    Seeded { balance: i32 },
}

/// One-time account bootstrap, run on a user's first authenticated request.
/// See module docs for the migrate-vs-seed decision. Idempotent.
pub async fn claim_account(
    store: &CreditStore,
    user_id: &str,
    device_key: Option<&str>,
) -> ClaimOutcome {
    match store {
        CreditStore::Db(pool) => claim_account_db(pool, user_id, device_key).await,
        CreditStore::File(state) => claim_account_file(state, user_id, device_key).await,
    }
}

async fn claim_account_db(
    pool: &PgPool,
    user_id: &str,
    device_key: Option<&str>,
) -> ClaimOutcome {
    let key = account_key(user_id);
    // A transaction with row locks makes the check-then-write atomic, so two
    // concurrent first-requests for the same account can't both seed/migrate.
    let result: Result<ClaimOutcome, sqlx::Error> = async {
        let mut tx = pool.begin().await?;

        // Does the account already exist? Lock the row if so.
        let existing: Option<i32> =
            sqlx::query_scalar("SELECT balance FROM public.credits WHERE id = $1 FOR UPDATE")
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(balance) = existing {
            tx.commit().await?;
            return Ok(ClaimOutcome::AlreadyClaimed { balance });
        }

        // New account. Check the device balance (locking it) for migration.
        let device_balance: Option<i32> = if let Some(d) = device_key {
            sqlx::query_scalar("SELECT balance FROM public.credits WHERE id = $1 FOR UPDATE")
                .bind(d)
                .fetch_optional(&mut *tx)
                .await?
        } else {
            None
        };

        let outcome = if let Some(amount) = device_balance.filter(|&b| b > 0) {
            let device = device_key.expect("amount implies device_key");
            sqlx::query(
                "INSERT INTO public.credits (id, balance) VALUES ($1, $2)",
            )
            .bind(&key)
            .bind(amount)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE public.credits SET balance = 0, updated_at = now() WHERE id = $1",
            )
            .bind(device)
            .execute(&mut *tx)
            .await?;
            info!(
                "Claimed account {} — migrated {} credits from device {}",
                short_id(user_id),
                amount,
                short_id(device)
            );
            ClaimOutcome::Migrated { from_device: amount, balance: amount }
        } else {
            sqlx::query("INSERT INTO public.credits (id, balance) VALUES ($1, $2)")
                .bind(&key)
                .bind(FREE_TIER_CREDITS)
                .execute(&mut *tx)
                .await?;
            info!(
                "Claimed account {} — seeded with {} free credits",
                short_id(user_id),
                FREE_TIER_CREDITS
            );
            ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS }
        };
        tx.commit().await?;
        Ok(outcome)
    }
    .await;

    result.unwrap_or_else(|e| {
        error!("Credits: claim_account DB error ({e}); treating as seeded fallback");
        // Safe fallback: report the free tier without persisting. The next
        // real operation will reconcile. Never grant a migrated balance on error.
        ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS }
    })
}

async fn claim_account_file(
    state: &Arc<Mutex<FileState>>,
    user_id: &str,
    device_key: Option<&str>,
) -> ClaimOutcome {
    let key = account_key(user_id);
    let mut s = state.lock().await;
    if let Some(&existing) = s.balances.get(&key) {
        return ClaimOutcome::AlreadyClaimed { balance: existing };
    }
    let migrate_amount = device_key
        .and_then(|d| s.balances.get(d).copied())
        .filter(|&bal| bal > 0);
    let outcome = if let Some(amount) = migrate_amount {
        let device = device_key.expect("migrate_amount implies device_key");
        s.balances.insert(key.clone(), amount);
        s.balances.insert(device.to_string(), 0);
        info!(
            "Claimed account {} — migrated {} credits from device {}",
            short_id(user_id),
            amount,
            short_id(device)
        );
        ClaimOutcome::Migrated { from_device: amount, balance: amount }
    } else {
        s.balances.insert(key.clone(), FREE_TIER_CREDITS);
        info!(
            "Claimed account {} — seeded with {} free credits",
            short_id(user_id),
            FREE_TIER_CREDITS
        );
        ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS }
    };
    persist(&s);
    outcome
}

/// Returns the identity's current balance, seeding the free tier if unseen
/// (preserves the original seed-on-read behavior for both backends).
pub async fn balance(store: &CreditStore, id: &str) -> i32 {
    match store {
        CreditStore::Db(pool) => {
            let res: Result<i32, sqlx::Error> = sqlx::query_scalar(
                "INSERT INTO public.credits (id, balance) VALUES ($1, $2)
                 ON CONFLICT (id) DO UPDATE SET updated_at = now()
                 RETURNING balance",
            )
            .bind(id)
            .bind(FREE_TIER_CREDITS)
            .fetch_one(pool)
            .await;
            res.unwrap_or_else(|e| {
                error!("Credits: balance DB error ({e}); reporting 0");
                0
            })
        }
        CreditStore::File(state) => {
            let mut s = state.lock().await;
            let was_new = !s.balances.contains_key(id);
            let bal = *s.balances.entry(id.to_string()).or_insert(FREE_TIER_CREDITS);
            if was_new {
                persist(&s);
            }
            bal
        }
    }
}

/// Atomically decrement an identity's balance by 1; `Err(())` if balance ≤ 0.
/// Seeds the free tier if unseen (so a first request from a never-claimed
/// identity is still allowed — matches legacy behavior).
pub async fn reserve(store: &CreditStore, id: &str) -> Result<i32, ()> {
    match store {
        CreditStore::Db(pool) => {
            let outcome: Result<Option<i32>, sqlx::Error> = async {
                let mut tx = pool.begin().await?;
                // Seed free tier if this identity has never been seen.
                sqlx::query(
                    "INSERT INTO public.credits (id, balance) VALUES ($1, $2)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(id)
                .bind(FREE_TIER_CREDITS)
                .execute(&mut *tx)
                .await?;
                // Atomic guarded decrement: only succeeds while balance > 0.
                let new: Option<i32> = sqlx::query_scalar(
                    "UPDATE public.credits SET balance = balance - 1, updated_at = now()
                     WHERE id = $1 AND balance > 0
                     RETURNING balance",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(new)
            }
            .await;
            match outcome {
                Ok(Some(bal)) => {
                    info!("Reserved 1 credit for {} — balance now {}", short_id(id), bal);
                    Ok(bal)
                }
                Ok(None) => Err(()), // balance was 0
                Err(e) => {
                    error!("Credits: reserve DB error ({e}); denying to be safe");
                    Err(())
                }
            }
        }
        CreditStore::File(state) => {
            let mut s = state.lock().await;
            {
                let bal = s.balances.entry(id.to_string()).or_insert(FREE_TIER_CREDITS);
                if *bal <= 0 {
                    return Err(());
                }
                *bal -= 1;
                info!("Reserved 1 credit for {} — balance now {}", short_id(id), *bal);
            }
            let new = *s.balances.get(id).unwrap_or(&0);
            persist(&s);
            Ok(new)
        }
    }
}

/// Refund a reservation (job ended Failed/Cancelled). +1, creating the row if
/// absent (matches legacy behavior where an absent id refunds to 1).
pub async fn refund(store: &CreditStore, id: &str) {
    match store {
        CreditStore::Db(pool) => {
            let res = sqlx::query_scalar::<_, i32>(
                "INSERT INTO public.credits (id, balance) VALUES ($1, 1)
                 ON CONFLICT (id) DO UPDATE SET balance = credits.balance + 1, updated_at = now()
                 RETURNING balance",
            )
            .bind(id)
            .fetch_one(pool)
            .await;
            match res {
                Ok(bal) => info!("Refunded 1 credit to {} — balance now {}", short_id(id), bal),
                Err(e) => error!("Credits: refund DB error for {} ({e})", short_id(id)),
            }
        }
        CreditStore::File(state) => {
            let mut s = state.lock().await;
            {
                let bal = s.balances.entry(id.to_string()).or_insert(0);
                *bal += 1;
                info!("Refunded 1 credit to {} — balance now {}", short_id(id), *bal);
            }
            persist(&s);
        }
    }
}

/// Add credits (Stripe webhook on `checkout.session.completed`).
pub async fn add(store: &CreditStore, id: &str, amount: i32) -> i32 {
    match store {
        CreditStore::Db(pool) => {
            let res = sqlx::query_scalar::<_, i32>(
                "INSERT INTO public.credits (id, balance) VALUES ($1, $2)
                 ON CONFLICT (id) DO UPDATE SET balance = credits.balance + $2, updated_at = now()
                 RETURNING balance",
            )
            .bind(id)
            .bind(amount)
            .fetch_one(pool)
            .await;
            match res {
                Ok(bal) => {
                    info!("Added {} credits to {} — balance now {}", amount, short_id(id), bal);
                    bal
                }
                Err(e) => {
                    error!("Credits: add DB error for {} ({e})", short_id(id));
                    0
                }
            }
        }
        CreditStore::File(state) => {
            let mut s = state.lock().await;
            let new = {
                let bal = s.balances.entry(id.to_string()).or_insert(0);
                *bal += amount;
                info!("Added {} credits to {} — balance now {}", amount, short_id(id), *bal);
                *bal
            };
            persist(&s);
            new
        }
    }
}

/// Lightweight `device_id` validation — alphanumeric, dashes, max 128 chars.
pub fn is_valid_device_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Short hash for logs so we don't leak full ids into Fly logs.
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

    /// File-backed test store pointed at a temp file.
    fn test_store(initial: &[(&str, i32)]) -> CreditStore {
        let mut balances = HashMap::new();
        for (k, v) in initial {
            balances.insert(k.to_string(), *v);
        }
        let mut path = std::env::temp_dir();
        path.push(format!("credits-test-{}.json", balances.len()));
        CreditStore::File(Arc::new(Mutex::new(FileState { balances, path })))
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
        assert_eq!(outcome, ClaimOutcome::Migrated { from_device: 34, balance: 34 });
        assert_eq!(balance(&store, &account_key("user-uuid-2")).await, 34);
        assert_eq!(balance(&store, "device-abc").await, 0);
    }

    #[tokio::test]
    async fn claim_is_idempotent() {
        let store = test_store(&[]);
        let first = claim_account(&store, "user-uuid-3", None).await;
        assert_eq!(first, ClaimOutcome::Seeded { balance: FREE_TIER_CREDITS });
        let second = claim_account(&store, "user-uuid-3", Some("device-xyz")).await;
        assert_eq!(second, ClaimOutcome::AlreadyClaimed { balance: FREE_TIER_CREDITS });
    }

    #[tokio::test]
    async fn claim_with_empty_device_seeds_free_tier() {
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
