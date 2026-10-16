use std::env;
use std::net::SocketAddr;

use axum::{
    extract::DefaultBodyLimit,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use scm_lp_optimizer::qa::answer_question;
use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{
    AskRequestPayload, AskResponsePayload, OptimizeRequestPayload, OptimizeResponsePayload,
};

const UI_HTML: &str = include_str!("../ui.html");

const EXAMPLES_JSON: &str = r#"{
  "shortage_bottleneck": {
    "config": {
      "holding_cost": 1.0,
      "shortage_penalty": 1000.0,
      "procurement_cost": 0.1,
      "production_cost": 5.0,
      "replenishment_cost": 2.0,
      "ss_violation_penalty": 100.0,
      "backlog_penalty_per_week": 50.0,
      "plan_stability_lambda": 0.0,
      "profit_maximization": false,
      "terminal_lookback_weeks": 4,
      "formulation_mode": "scipy"
    },
    "facts": [
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-02-02", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-02-02", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 120.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 240.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 200.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-02-02", "sales_qty": 140.0, "production_qty": 0, "replenishment_qty": 0 }
    ],
    "lines": [
      {
        "location": "PLANT-A",
        "line_id": "LINE-FG",
        "max_capacity": 160.0,
        "available_hours_per_week": 80.0
      }
    ],
    "bom": [
      {
        "target_location": "PLANT-A",
        "parent_sku": "FG-1",
        "child_sku": "RAW-1",
        "quantity": 2.0,
        "line_id": "LINE-FG",
        "rate_per_hour": 2.0,
        "yield_rate": 1.0,
        "scrap_rate": 0.0
      }
    ],
    "transport_routes": [
      {
        "from_location": "PLANT-A",
        "to_location": "DC-EAST",
        "sku": "FG-1",
        "transport_lt": 1,
        "transport_cost_per_unit": 0.5,
        "min_shipping_qty": 10,
        "shipping_lot_size": 10,
        "quota_pct": null
      }
    ],
    "item_locations": [
      {
        "location": "PLANT-A",
        "sku": "RAW-1",
        "can_procure": true,
        "min_procurement_qty": 20,
        "purchase_lot_size": 20,
        "procurement_lt_weeks": 0,
        "procurement_unit_cost": 1.0
      },
      {
        "location": "PLANT-A",
        "sku": "FG-1",
        "production_lt": 0,
        "min_production_qty": 10,
        "production_lot_size": 10,
        "cost": 3.0,
        "price": 20.0
      },
      {
        "location": "DC-EAST",
        "sku": "FG-1",
        "safety_stock_weeks": 0.5,
        "price": 20.0
      }
    ],
    "locations": [
      {
        "location": "PLANT-A",
        "holding_cost": 0.5,
        "max_storage_capacity": 2000.0
      },
      {
        "location": "DC-EAST",
        "holding_cost": 0.8,
        "max_storage_capacity": 1000.0
      }
    ],
    "init_inventory": [
      {
        "location": "DC-EAST",
        "sku": "FG-1",
        "qty": 40.0
      }
    ],
    "scheduled_receipts": [
      {
        "location": "PLANT-A",
        "sku": "RAW-1",
        "due_week": "2026-01-05",
        "qty": 100.0
      }
    ],
    "backorder_skus": [],
    "capacity_overrides": []
  },
  "basic_2echelon": {
    "config": {
      "holding_cost": 1.0,
      "shortage_penalty": 1000.0,
      "procurement_cost": 0.1,
      "production_cost": 5.0,
      "replenishment_cost": 2.0,
      "ss_violation_penalty": 100.0,
      "backlog_penalty_per_week": 50.0,
      "plan_stability_lambda": 0.0,
      "profit_maximization": false,
      "terminal_lookback_weeks": 4,
      "formulation_mode": "scipy"
    },
    "facts": [
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 100.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 150.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 120.0, "production_qty": 0, "replenishment_qty": 0 }
    ],
    "lines": [
      {
        "location": "PLANT-A",
        "line_id": "LINE-FG",
        "max_capacity": 180.0,
        "available_hours_per_week": 90.0
      }
    ],
    "bom": [
      {
        "target_location": "PLANT-A",
        "parent_sku": "FG-1",
        "child_sku": "RAW-1",
        "quantity": 2.0,
        "line_id": "LINE-FG",
        "rate_per_hour": 2.0,
        "yield_rate": 1.0,
        "scrap_rate": 0.0
      }
    ],
    "transport_routes": [
      {
        "from_location": "PLANT-A",
        "to_location": "DC-EAST",
        "sku": "FG-1",
        "transport_lt": 1,
        "transport_cost_per_unit": 0.5,
        "min_shipping_qty": 10,
        "shipping_lot_size": 10,
        "quota_pct": null
      }
    ],
    "item_locations": [
      {
        "location": "PLANT-A",
        "sku": "RAW-1",
        "can_procure": true,
        "min_procurement_qty": 20,
        "purchase_lot_size": 20,
        "procurement_lt_weeks": 0,
        "procurement_unit_cost": 1.0
      },
      {
        "location": "PLANT-A",
        "sku": "FG-1",
        "production_lt": 0,
        "min_production_qty": 10,
        "production_lot_size": 10,
        "cost": 3.0,
        "price": 20.0
      },
      {
        "location": "DC-EAST",
        "sku": "FG-1",
        "safety_stock_weeks": 0.5,
        "price": 20.0
      }
    ],
    "locations": [
      {
        "location": "PLANT-A",
        "holding_cost": 0.5,
        "max_storage_capacity": 2000.0
      },
      {
        "location": "DC-EAST",
        "holding_cost": 0.8,
        "max_storage_capacity": 1000.0
      }
    ],
    "init_inventory": [
      {
        "location": "DC-EAST",
        "sku": "FG-1",
        "qty": 50.0
      }
    ],
    "scheduled_receipts": [
      {
        "location": "PLANT-A",
        "sku": "RAW-1",
        "due_week": "2026-01-05",
        "qty": 100.0
      }
    ],
    "backorder_skus": [],
    "capacity_overrides": []
  },
  "multi_plant_quota": {
    "config": {
      "holding_cost": 1.0,
      "shortage_penalty": 1000.0,
      "procurement_cost": 0.1,
      "production_cost": 5.0,
      "replenishment_cost": 2.0,
      "ss_violation_penalty": 100.0,
      "backlog_penalty_per_week": 50.0,
      "plan_stability_lambda": 0.0,
      "profit_maximization": false,
      "terminal_lookback_weeks": 4,
      "formulation_mode": "scipy"
    },
    "facts": [
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "RAW-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "PLANT-B", "sku": "RAW-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "RAW-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "RAW-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "RAW-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-A", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "PLANT-B", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "PLANT-B", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 120.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 180.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-EAST", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 160.0, "production_qty": 0, "replenishment_qty": 0 },

      { "location": "DC-WEST", "sku": "FG-1", "week_start": "2026-01-05", "sales_qty": 0.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-WEST", "sku": "FG-1", "week_start": "2026-01-12", "sales_qty": 80.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-WEST", "sku": "FG-1", "week_start": "2026-01-19", "sales_qty": 120.0, "production_qty": 0, "replenishment_qty": 0 },
      { "location": "DC-WEST", "sku": "FG-1", "week_start": "2026-01-26", "sales_qty": 100.0, "production_qty": 0, "replenishment_qty": 0 }
    ],
    "lines": [
      {
        "location": "PLANT-A",
        "line_id": "LINE-A1",
        "max_capacity": 200.0,
        "available_hours_per_week": 100.0
      },
      {
        "location": "PLANT-B",
        "line_id": "LINE-B1",
        "max_capacity": 160.0,
        "available_hours_per_week": 80.0
      }
    ],
    "bom": [
      {
        "target_location": "PLANT-A",
        "parent_sku": "FG-1",
        "child_sku": "RAW-1",
        "quantity": 2.0,
        "line_id": "LINE-A1",
        "rate_per_hour": 2.0,
        "yield_rate": 1.0,
        "scrap_rate": 0.0
      },
      {
        "target_location": "PLANT-B",
        "parent_sku": "FG-1",
        "child_sku": "RAW-1",
        "quantity": 2.0,
        "line_id": "LINE-B1",
        "rate_per_hour": 2.0,
        "yield_rate": 1.0,
        "scrap_rate": 0.0
      }
    ],
    "transport_routes": [
      {
        "from_location": "PLANT-A",
        "to_location": "DC-EAST",
        "sku": "FG-1",
        "transport_lt": 1,
        "transport_cost_per_unit": 0.5,
        "min_shipping_qty": 10,
        "shipping_lot_size": 10,
        "quota_pct": 0.6
      },
      {
        "from_location": "PLANT-B",
        "to_location": "DC-EAST",
        "sku": "FG-1",
        "transport_lt": 1,
        "transport_cost_per_unit": 0.7,
        "min_shipping_qty": 10,
        "shipping_lot_size": 10,
        "quota_pct": 0.4
      },
      {
        "from_location": "PLANT-B",
        "to_location": "DC-WEST",
        "sku": "FG-1",
        "transport_lt": 1,
        "transport_cost_per_unit": 0.4,
        "min_shipping_qty": 10,
        "shipping_lot_size": 10,
        "quota_pct": null
      }
    ],
    "item_locations": [
      {
        "location": "PLANT-A",
        "sku": "RAW-1",
        "can_procure": true,
        "min_procurement_qty": 20,
        "purchase_lot_size": 20,
        "procurement_lt_weeks": 0,
        "procurement_unit_cost": 1.0
      },
      {
        "location": "PLANT-B",
        "sku": "RAW-1",
        "can_procure": true,
        "min_procurement_qty": 20,
        "purchase_lot_size": 20,
        "procurement_lt_weeks": 0,
        "procurement_unit_cost": 1.1
      },
      {
        "location": "PLANT-A",
        "sku": "FG-1",
        "production_lt": 0,
        "min_production_qty": 10,
        "production_lot_size": 10,
        "cost": 3.0,
        "price": 20.0
      },
      {
        "location": "PLANT-B",
        "sku": "FG-1",
        "production_lt": 0,
        "min_production_qty": 10,
        "production_lot_size": 10,
        "cost": 3.2,
        "price": 20.0
      },
      {
        "location": "DC-EAST",
        "sku": "FG-1",
        "safety_stock_weeks": 0.5,
        "price": 20.0
      },
      {
        "location": "DC-WEST",
        "sku": "FG-1",
        "safety_stock_weeks": 0.5,
        "price": 20.0
      }
    ],
    "locations": [
      {
        "location": "PLANT-A",
        "holding_cost": 0.5,
        "max_storage_capacity": 2000.0
      },
      {
        "location": "PLANT-B",
        "holding_cost": 0.5,
        "max_storage_capacity": 2000.0
      },
      {
        "location": "DC-EAST",
        "holding_cost": 0.8,
        "max_storage_capacity": 1000.0
      },
      {
        "location": "DC-WEST",
        "holding_cost": 0.8,
        "max_storage_capacity": 1000.0
      }
    ],
    "init_inventory": [
      { "location": "DC-EAST", "sku": "FG-1", "qty": 60.0 },
      { "location": "DC-WEST", "sku": "FG-1", "qty": 40.0 }
    ],
    "scheduled_receipts": [
      { "location": "PLANT-A", "sku": "RAW-1", "due_week": "2026-01-05", "qty": 100.0 },
      { "location": "PLANT-B", "sku": "RAW-1", "due_week": "2026-01-05", "qty": 100.0 }
    ],
    "backorder_skus": [],
    "capacity_overrides": []
  }
}"#;

async fn index_handler() -> Html<&'static str> {
    Html(UI_HTML)
}

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

async fn examples_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        EXAMPLES_JSON,
    )
}

async fn optimize_handler(
    Json(req): Json<Box<OptimizeRequestPayload>>,
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
                "edits": [],
                "line_utilizations": [],
                "transport_flows": [],
                "sku_psi_details": [],
                "root_causes": []
            })),
        )),
    }
}

async fn ask_handler(
    Json(req): Json<Box<AskRequestPayload>>,
) -> Result<(StatusCode, Json<AskResponsePayload>), (StatusCode, Json<serde_json::Value>)> {
    let res = tokio::task::spawn_blocking(move || answer_question(&req))
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
                "error_msg": err.to_string()
            })),
        )),
    }
}

fn main() {
    let builder = std::thread::Builder::new()
        .name("scm-server-main".into())
        .stack_size(16 * 1024 * 1024);

    let handle = builder
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(16 * 1024 * 1024)
                .build()
                .expect("Failed to build Tokio runtime");

            rt.block_on(async {
                let port: u16 = env::var("PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(8080);

                let app = Router::new()
                    .route("/", get(index_handler))
                    .route("/health", get(health_handler))
                    .route("/examples", get(examples_handler))
                    .route("/optimize", post(optimize_handler))
                    .route("/ask", post(ask_handler))
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
            });
        })
        .expect("Failed to spawn server main thread");

    handle.join().expect("Server main thread panicked");
}
