//! Parity tests for the data-driven openSCM features: alternate lines, firm cells (`is_fixed`),
//! dynamic sourcing, storage UOM, ghost suppliers and projected lot expiries.

use std::collections::HashMap;

use scm_lp_optimizer::lp_model::{build_lp_model, formulate_lp};
use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{
    BomRow, FactRow, InitInventoryRow, InitLotRow, ItemLocationRow, LineEligibilityRow,
    LocationRow, OptimizeRequestPayload, ProductionLineRow, TransportRouteRow, UomFactorRow,
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

fn prod_at(resp: &scm_lp_optimizer::schema::OptimizeResponsePayload, week: &str) -> i64 {
    resp.facts
        .iter()
        .find(|f| f.location == "PLANT" && f.sku == "FG" && f.week_start == week)
        .map(|f| f.production_qty)
        .unwrap()
}

#[test]
fn alternate_line_covers_primary_line_shortfall() {
    let mut req = base_request(100.0);
    req.lines.push(line("PLANT", "L2", 100.0));
    req.line_eligibility = vec![LineEligibilityRow {
        location: "PLANT".to_string(),
        sku: "FG".to_string(),
        line_id: "L2".to_string(),
        efficiency_rate: Some(0.5),
    }];
    let resp = run_optimization(&req).unwrap();
    // L1 gives 50, L2 (efficiency 0.5 -> 2 capacity per unit) gives another 50.
    let usage: f64 = resp.alt_line_usage.iter().map(|r| r.qty).sum();
    assert!((usage - 50.0).abs() < 1e-2, "{:?}", resp.alt_line_usage);
    assert_eq!(prod_at(&resp, "2026-01-05"), 100);
    assert!(resp.warnings.iter().any(|w| w.code == "alt_line_split_not_persisted"));

    // Without eligibility the plan is capped at the primary line.
    let mut no_alt = base_request(100.0);
    no_alt.lines.push(line("PLANT", "L2", 100.0));
    let resp = run_optimization(&no_alt).unwrap();
    assert!(prod_at(&resp, "2026-01-05") <= 50);
    assert!(resp.alt_line_usage.is_empty());
}

#[test]
fn firm_cell_is_pinned_and_infeasible_firm_is_relaxed() {
    // Firm 30 units in week 1 with no demand: the plan must keep them.
    let mut req = base_request(0.0);
    req.facts[2].is_fixed = true; // PLANT/FG week 2026-01-12
    req.facts[2].production_qty = 30;
    let resp = run_optimize_ok(&req);
    assert_eq!(prod_at(&resp, "2026-01-12"), 30);
    assert!(resp.warnings.iter().all(|w| w.code != "firm_qty_not_feasible"));

    // Firm 80 > line capacity 50: infeasible as pinned -> relaxed retry + warning.
    let mut req = base_request(0.0);
    req.facts[2].is_fixed = true;
    req.facts[2].production_qty = 80;
    let resp = run_optimize_ok(&req);
    // Like openSCM the firmed quantity is restored in the output; the warning flags it.
    assert_eq!(prod_at(&resp, "2026-01-12"), 80);
    assert!(
        resp.warnings.iter().any(|w| w.code == "firm_qty_not_feasible"),
        "{:?}",
        resp.warnings
    );
}

fn run_optimize_ok(req: &OptimizeRequestPayload) -> scm_lp_optimizer::schema::OptimizeResponsePayload {
    run_optimization(req).expect("optimization should succeed")
}

#[test]
fn storage_capacity_uses_uom_coefficient() {
    let mut req = base_request(10.0);
    req.locations = vec![LocationRow {
        location: "PLANT".to_string(),
        holding_cost: None,
        holding_cost_rate: None,
        max_storage_capacity: Some(10.0),
        allocation_priority: None,
        storage_capacity_uom: Some("case".to_string()),
    }];
    req.uom_factors = vec![UomFactorRow {
        sku: "FG".to_string(),
        uom: "CASE".to_string(),
        factor: 10.0, // 1 case = 10 base units -> coefficient 0.1
    }];
    let model = build_lp_model(&req).unwrap().unwrap();
    let lp = formulate_lp(&model, &req.config);
    let storage_rows: Vec<_> = lp
        .ub_rows
        .iter()
        .filter(|r| r.bottleneck_desc.as_deref().is_some_and(|d| d.starts_with("Storage Capacity")))
        .collect();
    assert_eq!(storage_rows.len(), WEEKS.len());
    let coeffs: Vec<f64> = storage_rows[0].terms.iter().map(|&(_, c)| c).collect();
    assert!(coeffs.iter().any(|c| (c - 0.1).abs() < 1e-12), "{coeffs:?}"); // FG
    assert!(coeffs.iter().any(|c| (c - 1.0).abs() < 1e-12), "{coeffs:?}"); // RAW has no factor
}

#[test]
fn ghost_supplier_pair_is_added_without_supply_variables() {
    let mut req = base_request(10.0);
    // RAW is made/stored at SUPPLIER which has no facts rows.
    req.bom = vec![BomRow {
        target_location: "PLANT".to_string(),
        parent_sku: "FG".to_string(),
        child_sku: "RAW".to_string(),
        quantity: 2.0,
        line_id: Some("L1".to_string()),
        rate_per_hour: None,
        yield_rate: None,
        scrap_rate: None,
    }];
    req.transport_routes = vec![TransportRouteRow {
        from_location: "SUPPLIER".to_string(),
        to_location: "PLANT".to_string(),
        sku: "RAW".to_string(),
        transport_lt: Some(0),
        transport_cost_per_unit: Some(1.0),
        min_shipping_qty: None,
        shipping_lot_size: None,
        quota_pct: None,
    }];
    req.init_inventory.push(InitInventoryRow {
        location: "SUPPLIER".to_string(),
        sku: "RAW".to_string(),
        qty: 5_000.0,
    });
    let model = build_lp_model(&req).unwrap().unwrap();
    let ghost = model.pair_idx_map[&("SUPPLIER".to_string(), "RAW".to_string())];
    assert!(model.ghost_pairs.contains(&ghost));
    assert!(!model.all_pairs[ghost].can_produce && !model.all_pairs[ghost].can_replenish);
    assert!(model.warnings.iter().any(|w| w.code == "supplier_pair_missing_in_facts"));
    run_optimize_ok(&req);
}

#[test]
fn projected_lot_expiry_enters_balance_rhs() {
    let mut req = base_request(0.0);
    req.facts[0].sales_qty = 0.0;
    req.init_inventory.push(InitInventoryRow {
        location: "PLANT".to_string(),
        sku: "FG".to_string(),
        qty: 40.0,
    });
    // 40 units expire in week index 2 (2026-01-19); no demand consumes them first.
    req.init_lots = vec![InitLotRow {
        location: "PLANT".to_string(),
        sku: "FG".to_string(),
        qty: 40.0,
        expiry_week: Some("2026-01-19".to_string()),
    }];
    let model = build_lp_model(&req).unwrap().unwrap();
    let fg = model.pair_idx_map[&("PLANT".to_string(), "FG".to_string())];
    let ex = model.projected_expiries.as_ref().expect("expiries");
    assert!((ex[fg][2] - 40.0).abs() < 1e-12, "{:?}", ex[fg]);
    assert_eq!(ex[fg][0], 0.0);
}

#[test]
fn dynamic_sourcing_keeps_all_routes_as_edges() {
    let mut req = base_request(0.0);
    for w in WEEKS {
        req.facts.push(fact("DC", "FG", w, 0.0));
        req.facts.push(fact("PLANT2", "FG", w, 0.0));
    }
    req.lines.push(line("PLANT2", "L9", 50.0));
    req.bom.push(bom("PLANT2", "FG", "RAW", 2.0, "L9"));
    req.item_locations = vec![ItemLocationRow {
        location: "PLANT".to_string(),
        sku: "FG".to_string(),
        ..Default::default()
    }];
    let route = |from: &str, lt: i64, cost: f64| TransportRouteRow {
        from_location: from.to_string(),
        to_location: "DC".to_string(),
        sku: "FG".to_string(),
        transport_lt: Some(lt),
        transport_cost_per_unit: Some(cost),
        min_shipping_qty: None,
        shipping_lot_size: None,
        quota_pct: None,
    };
    req.transport_routes = vec![route("PLANT", 0, 1.0), route("PLANT2", 1, 2.0)];

    let static_model = build_lp_model(&req).unwrap().unwrap();
    assert_eq!(static_model.transport_edges.len(), 1); // winner only
    assert!(static_model.dynamic_transport_edges.is_empty());

    req.config.dynamic_sourcing = true;
    let dyn_model = build_lp_model(&req).unwrap().unwrap();
    assert_eq!(dyn_model.transport_edges.len(), 2);
    assert_eq!(dyn_model.dynamic_transport_edges.len(), 2);
}

fn route(from: &str, to: &str, lt: i64, cost: f64) -> TransportRouteRow {
    TransportRouteRow {
        from_location: from.to_string(),
        to_location: to.to_string(),
        sku: "FG".to_string(),
        transport_lt: Some(lt),
        transport_cost_per_unit: Some(cost),
        min_shipping_qty: None,
        shipping_lot_size: None,
        quota_pct: None,
    }
}

fn total_prod(resp: &scm_lp_optimizer::schema::OptimizeResponsePayload, loc: &str) -> i64 {
    resp.facts
        .iter()
        .filter(|f| f.location == loc && f.sku == "FG")
        .map(|f| f.production_qty)
        .sum()
}

#[test]
fn relaxed_firm_replenishment_still_pulls_upstream_production() {
    // DC has a firmed shipment (300) that the plant (4 weeks x 50) cannot fully supply.
    let mut req = base_request(0.0);
    for w in WEEKS {
        req.facts.push(fact("DC", "FG", w, 0.0));
    }
    let dc_t1 = req
        .facts
        .iter_mut()
        .find(|f| f.location == "DC" && f.week_start == WEEKS[1])
        .unwrap();
    dc_t1.is_fixed = true;
    dc_t1.replenishment_qty = 300;
    req.transport_routes = vec![route("PLANT", "DC", 0, 1.0)];

    let resp = run_optimize_ok(&req);
    assert!(
        resp.warnings.iter().any(|w| w.code == "firm_qty_not_feasible"),
        "{:?}",
        resp.warnings
    );
    // The relaxed solve must keep the reward on the protected R cell, so the plant produces.
    assert!(total_prod(&resp, "PLANT") > 0, "plant plans nothing behind the firm shipment");
}

#[test]
fn dynamic_sourcing_uses_second_plant_end_to_end() {
    let mut req = base_request(0.0);
    for w in WEEKS {
        req.facts.push(fact("DC", "FG", w, if w == WEEKS[1] { 140.0 } else { 0.0 }));
        req.facts.push(fact("PLANT2", "FG", w, 0.0));
        req.facts.push(fact("PLANT2", "RAW", w, 0.0));
    }
    req.lines.push(line("PLANT2", "L9", 50.0));
    req.bom.push(bom("PLANT2", "FG", "RAW", 2.0, "L9"));
    req.init_inventory.push(InitInventoryRow {
        location: "PLANT2".to_string(),
        sku: "RAW".to_string(),
        qty: 10_000.0,
    });
    req.transport_routes = vec![route("PLANT", "DC", 0, 1.0), route("PLANT2", "DC", 1, 2.0)];
    // Zero holding cost: otherwise burning the large RAW stock (holding 1.0/unit/week) pays for
    // itself and every plant produces regardless of demand.
    req.locations = ["PLANT", "PLANT2", "DC"]
        .iter()
        .map(|l| LocationRow {
            location: l.to_string(),
            holding_cost: Some(0.0),
            holding_cost_rate: None,
            max_storage_capacity: None,
            allocation_priority: None,
            storage_capacity_uom: None,
        })
        .collect();

    // Static: only the winning route (PLANT) exists -> PLANT2 never produces.
    let resp = run_optimize_ok(&req);
    let static_p2 = total_prod(&resp, "PLANT2");
    assert!(static_p2 < 10, "static plant2 production = {static_p2}");

    // Dynamic: the 40 units PLANT cannot make in time are sourced from PLANT2.
    req.config.dynamic_sourcing = true;
    let resp = run_optimize_ok(&req);
    assert!(
        total_prod(&resp, "PLANT2") >= 30,
        "plant2 production = {}, plant = {}",
        total_prod(&resp, "PLANT2"),
        total_prod(&resp, "PLANT")
    );
}
