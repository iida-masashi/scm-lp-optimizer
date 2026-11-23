#![cfg(feature = "highs")]
//! MILP backend (`config.solver = "milp"`): lot-size integers, minimum batch and setup cost.

use std::collections::HashMap;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{
    BomRow, FactRow, InitInventoryRow, ItemLocationRow, OptimizeRequestPayload, ProductionLineRow,
};

const WEEKS: [&str; 4] = ["2026-01-05", "2026-01-12", "2026-01-19", "2026-01-26"];

fn fact(loc: &str, sku: &str, week: &str, sales: f64) -> FactRow {
    FactRow {
        is_fixed: false,
        location: loc.to_string(),
        sku: sku.to_string(),
        week_start: week.to_string(),
        sales_qty: sales,
        dependent_demand_qty: 0.0,
        production_qty: 0,
        replenishment_qty: 0,
        production_request: None,
        replenishment_request: None,
        order_release_qty: None,
        safety_stock_weeks: None,
        production_fence_weeks: None,
        replenishment_fence_weeks: None,
        extra: HashMap::new(),
    }
}

fn line(loc: &str, id: &str, cap: f64) -> ProductionLineRow {
    ProductionLineRow {
        location: loc.to_string(),
        line_id: id.to_string(),
        max_capacity: cap,
        available_hours_per_week: None,
    }
}

fn bom(loc: &str, parent: &str, child: &str, qty: f64, line_id: &str) -> BomRow {
    BomRow {
        target_location: loc.to_string(),
        parent_sku: parent.to_string(),
        child_sku: child.to_string(),
        quantity: qty,
        line_id: Some(line_id.to_string()),
        rate_per_hour: None,
        yield_rate: None,
        scrap_rate: None,
        valid_from: None,
        valid_to: None,
    }
}

/// PLANT makes FG (2 x RAW) on L1; demand is injected by the caller.
fn base_request(demand_week0: f64) -> OptimizeRequestPayload {
    let mut facts = Vec::new();
    for (t, w) in WEEKS.iter().enumerate() {
        let d = if t == 0 { demand_week0 } else { 0.0 };
        facts.push(fact("PLANT", "FG", w, d));
        facts.push(fact("PLANT", "RAW", w, 0.0));
    }
    OptimizeRequestPayload {
        facts,
        lines: vec![line("PLANT", "L1", 50.0)],
        bom: vec![bom("PLANT", "FG", "RAW", 2.0, "L1")],
        init_inventory: vec![InitInventoryRow {
            location: "PLANT".to_string(),
            sku: "RAW".to_string(),
            qty: 10_000.0,
        }],
        ..Default::default()
    }
}

fn item(loc: &str, sku: &str) -> ItemLocationRow {
    ItemLocationRow { location: loc.to_string(), sku: sku.to_string(), ..Default::default() }
}

fn all_prod(resp: &scm_lp_optimizer::schema::OptimizeResponsePayload) -> Vec<i64> {
    resp.facts
        .iter()
        .filter(|f| f.location == "PLANT" && f.sku == "FG")
        .map(|f| f.production_qty)
        .collect()
}

#[test]
fn milp_respects_lot_size_and_minimum_batch() {
    let mut req = base_request(25.0);
    req.config.solver = "milp".to_string();
    let mut it = item("PLANT", "FG");
    it.production_lot_size = Some(10);
    it.min_production_qty = Some(40);
    req.item_locations = vec![it];
    let resp = run_optimization(&req).unwrap();
    let prod = all_prod(&resp);
    assert!(prod.iter().all(|&q| q == 0 || (q % 10 == 0 && q >= 40)), "{prod:?}");
    assert!(prod.iter().sum::<i64>() >= 25, "{prod:?}");
}

#[test]
fn milp_setup_cost_consolidates_runs() {
    // Demand in two weeks; a high setup cost makes one early run cheaper than two.
    let mut req = base_request(0.0);
    for f in req.facts.iter_mut() {
        if f.sku == "FG" && (f.week_start == WEEKS[0] || f.week_start == WEEKS[2]) {
            f.sales_qty = 20.0;
        }
    }
    req.config.solver = "milp".to_string();
    req.config.holding_cost = 0.01;
    let mut it = item("PLANT", "FG");
    it.setup_cost = Some(10_000.0);
    req.item_locations = vec![it];
    let resp = run_optimization(&req).unwrap();
    let runs = all_prod(&resp).iter().filter(|&&q| q > 0).count();
    assert_eq!(runs, 1, "{:?}", all_prod(&resp));
}

#[test]
fn lp_solver_is_unchanged_by_default() {
    let mut req = base_request(25.0);
    let mut it = item("PLANT", "FG");
    it.setup_cost = Some(10_000.0);
    req.item_locations = vec![it];
    // setup_cost only matters for the MILP backend
    let resp = run_optimization(&req).unwrap();
    assert!(all_prod(&resp).iter().sum::<i64>() >= 25);
}
