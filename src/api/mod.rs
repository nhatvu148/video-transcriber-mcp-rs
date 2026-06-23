pub mod handlers;
pub mod jobs;
pub mod stripe;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

pub use handlers::AppState;
pub use jobs::new_store;

// 2 GB max upload. Whisper itself can churn through that just fine — the
// limit is mostly to refuse genuinely insane uploads.
const UPLOAD_MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/jobs", post(handlers::create_job))
        .route(
            "/jobs/{id}",
            get(handlers::get_job).delete(handlers::cancel_job),
        )
        .route(
            "/jobs/upload",
            post(handlers::upload_job).layer(DefaultBodyLimit::max(UPLOAD_MAX_BYTES)),
        )
        .route("/balance", get(handlers::get_balance))
        .route("/me", get(handlers::get_me))
        .route("/checkout", post(stripe::create_checkout))
        .route("/webhook/stripe", post(stripe::webhook))
        .with_state(state)
}
