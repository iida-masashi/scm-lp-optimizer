use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OptimizerError {
    #[error("LP solver error (status={status}): {message} ({solver_message})")]
    SolverError {
        message: String,
        status: i32,
        solver_message: String,
    },
    #[error("Invalid input data: {0}")]
    InvalidInput(String),
}

/// Configuration parameters for the LP objective and safety stock target calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolverConfig {
    #[serde(default = "default_holding_cost")]
    pub holding_cost: f64,
    #[serde(default = "default_shortage_penalty")]
    pub shortage_penalty: f64,
    #[serde(default = "default_procurement_cost")]
    pub procurement_cost: f64,
    #[serde(default = "default_production_cost")]
    pub production_cost: f64,
    #[serde(default = "default_replenishment_cost")]
    pub replenishment_cost: f64,
    #[serde(default = "default_ss_violation_penalty")]
    pub ss_violation_penalty: f64,
    #[serde(default = "default_backlog_penalty_per_week")]
    pub backlog_penalty_per_week: f64,
    #[serde(default = "default_plan_stability_lambda")]
    pub plan_stability_lambda: f64,
    #[serde(default)]
    pub profit_maximization: bool,
    pub terminal_lookback_weeks: Option<usize>,
    /// Solver backend behavior mode:
    /// - `"scipy"` (default): uses Network Design explicit `TR` edge variables when no quota split is active.
    /// - `"glop"`: uses direct `transport_consumers` multiplier coefficients on `R` variables.
    #[serde(default = "default_formulation_mode")]
    pub formulation_mode: String,
}

fn default_holding_cost() -> f64 { 1.0 }
fn default_shortage_penalty() -> f64 { 1000.0 }
fn default_procurement_cost() -> f64 { 0.1 }
fn default_production_cost() -> f64 { 5.0 }
fn default_replenishment_cost() -> f64 { 2.0 }
fn default_ss_violation_penalty() -> f64 { 100.0 }
fn default_backlog_penalty_per_week() -> f64 { 50.0 }
fn default_plan_stability_lambda() -> f64 { 0.0 }
fn default_formulation_mode() -> String { "scipy".to_string() }

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            holding_cost: default_holding_cost(),
            shortage_penalty: default_shortage_penalty(),
            procurement_cost: default_procurement_cost(),
            production_cost: default_production_cost(),
            replenishment_cost: default_replenishment_cost(),
            ss_violation_penalty: default_ss_violation_penalty(),
            backlog_penalty_per_week: default_backlog_penalty_per_week(),
            plan_stability_lambda: default_plan_stability_lambda(),
            profit_maximization: false,
            terminal_lookback_weeks: None,
            formulation_mode: default_formulation_mode(),
        }
    }
}

/// Weekly fact row for a (location, sku, week_start) cell.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FactRow {
    pub location: String,
    pub sku: String,
    pub week_start: String,
    #[serde(default)]
    pub sales_qty: f64,
    #[serde(default)]
    pub dependent_demand_qty: f64,
    #[serde(default)]
    pub production_qty: i64,
    #[serde(default)]
    pub replenishment_qty: i64,
    #[serde(default)]
    pub production_request: Option<i64>,
    #[serde(default)]
    pub replenishment_request: Option<i64>,
    #[serde(default)]
    pub order_release_qty: Option<i64>,
    #[serde(default)]
    pub safety_stock_weeks: Option<f64>,
    #[serde(default)]
    pub production_fence_weeks: Option<i64>,
    #[serde(default)]
    pub replenishment_fence_weeks: Option<i64>,
    /// Preserve any additional caller fields so write-back can round-trip them if needed.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Production line definition (`master_production_lines`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionLineRow {
    pub location: String,
    pub line_id: String,
    pub max_capacity: f64,
    #[serde(default)]
    pub available_hours_per_week: Option<f64>,
}

/// Bill of Materials row (`master_bom`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BomRow {
    pub target_location: String,
    pub parent_sku: String,
    pub child_sku: String,
    pub quantity: f64,
    #[serde(default)]
    pub line_id: Option<String>,
    #[serde(default)]
    pub rate_per_hour: Option<f64>,
    #[serde(default)]
    pub yield_rate: Option<f64>,
    #[serde(default)]
    pub scrap_rate: Option<f64>,
}

/// Transport route row (`master_transport_routes` after wildcard expansion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportRouteRow {
    pub from_location: String,
    pub to_location: String,
    pub sku: String,
    #[serde(default)]
    pub transport_lt: Option<i64>,
    #[serde(default)]
    pub transport_cost_per_unit: Option<f64>,
    #[serde(default)]
    pub min_shipping_qty: Option<i64>,
    #[serde(default)]
    pub shipping_lot_size: Option<i64>,
    #[serde(default)]
    pub quota_pct: Option<f64>,
}

/// Item-location master and planning parameter overrides (`master_item_locations` + `planning_parameters`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ItemLocationRow {
    pub location: String,
    pub sku: String,
    #[serde(default)]
    pub can_procure: Option<bool>,
    #[serde(default)]
    pub production_lt: Option<i64>,
    #[serde(default)]
    pub safety_lt_weeks: Option<i64>,
    #[serde(default)]
    pub min_production_qty: Option<i64>,
    #[serde(default)]
    pub production_lot_size: Option<i64>,
    #[serde(default)]
    pub min_procurement_qty: Option<i64>,
    #[serde(default)]
    pub purchase_lot_size: Option<i64>,
    #[serde(default)]
    pub weekly_procurement_capacity: Option<f64>,
    #[serde(default)]
    pub procurement_lt_weeks: Option<i64>,
    #[serde(default)]
    pub safety_stock_weeks: Option<f64>,
    #[serde(default)]
    pub excess_inventory_buffer_ratio: Option<f64>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub procurement_unit_cost: Option<f64>,
}

/// Location master (`master_locations`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationRow {
    pub location: String,
    #[serde(default)]
    pub holding_cost: Option<f64>,
    #[serde(default)]
    pub holding_cost_rate: Option<f64>,
    #[serde(default)]
    pub max_storage_capacity: Option<f64>,
}

/// Initial inventory balance per `(location, sku)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitInventoryRow {
    pub location: String,
    pub sku: String,
    pub qty: f64,
}

/// Scheduled receipt (`scheduled_receipts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledReceiptRow {
    pub location: String,
    pub sku: String,
    pub due_week: String,
    pub qty: f64,
}

/// Weekly line capacity override (`master_capacity_overrides`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityOverrideRow {
    pub location: String,
    pub line_id: String,
    pub week_start: String,
    pub max_capacity: f64,
}

/// Unconstrained Pass-1 request entry (`request_map`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMapEntry {
    pub location: String,
    pub sku: String,
    pub week_start: String,
    pub qty: f64,
}

/// Routing kind for unconstrained request (`request_kind_map`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestKindEntry {
    pub location: String,
    pub sku: String,
    /// Either `"production_qty"` or `"replenishment_qty"`
    pub kind: String,
}

/// Complete input payload for the LP optimization service.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OptimizeRequestPayload {
    #[serde(default)]
    pub config: SolverConfig,
    pub facts: Vec<FactRow>,
    #[serde(default)]
    pub lines: Vec<ProductionLineRow>,
    #[serde(default)]
    pub bom: Vec<BomRow>,
    #[serde(default)]
    pub init_inventory: Vec<InitInventoryRow>,
    #[serde(default)]
    pub transport_routes: Vec<TransportRouteRow>,
    #[serde(default)]
    pub item_locations: Vec<ItemLocationRow>,
    #[serde(default)]
    pub locations: Vec<LocationRow>,
    #[serde(default)]
    pub scheduled_receipts: Vec<ScheduledReceiptRow>,
    #[serde(default)]
    pub backorder_skus: Vec<String>,
    #[serde(default)]
    pub capacity_overrides: Vec<CapacityOverrideRow>,
    #[serde(default)]
    pub request_map: Vec<RequestMapEntry>,
    #[serde(default)]
    pub request_kind_map: Vec<RequestKindEntry>,
}

/// Dual-value capacity / storage bottleneck reported by the LP solver.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bottleneck {
    pub description: String,
    pub marginal_value: f64,
}

/// Delta edit row returned for convenience (matching `services/solver_runtime.py`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanEditRow {
    pub location: String,
    pub sku: String,
    pub week_start: String,
    pub production_qty: i64,
    pub replenishment_qty: i64,
    pub production_request: i64,
    pub replenishment_request: i64,
    pub order_release_qty: i64,
}

/// Complete output payload from the LP optimization service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizeResponsePayload {
    pub status: String,
    pub elapsed_ms: u64,
    pub facts: Vec<FactRow>,
    pub edits: Vec<PlanEditRow>,
    pub bottlenecks: Vec<Bottleneck>,
}
