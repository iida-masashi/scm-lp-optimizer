use std::env;
use std::net::SocketAddr;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{OptimizeRequestPayload, OptimizeResponsePayload};

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": "scm-lp-optimizer",
            "engine": "clarabel-pure-rust"
        })),
    )
}

async fn optimize_handler(
    Json(req): Json<OptimizeRequestPayload>,
) -> Result<(StatusCode, Json<OptimizeResponsePayload>), (StatusCode, Json<serde_json::Value>)> {
    // Run CPU-bound LP optimization on blocking thread pool
    let res = tokio::task::spawn_blocking(move || run_optimization(&req))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error_msg": format!("Task join error: {}", e)
                })),
            )
        })?;

    match res {
        Ok(payload) => Ok((StatusCode::OK, Json(payload))),
        Err(err) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "objective_value": null,
                "bottlenecks": [],
                "warnings": [],
                "error_msg": err.to_string(),
                "facts": [],
                "edits": []
            })),
        )),
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/optimize", post(optimize_handler))
        // Allow up to 64 MB payloads for large multi-year supply chain plans
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("scm-lp-server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind TCP listener");
    axum::serve(listener, app)
        .await
        .expect("Server execution error");
}
