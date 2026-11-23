//! Parity tests for the openSCM objective semantics (value-added cost, annual holding rate,
//! terminal inventory valuation, SS penalty, allocation priority, BOM yield).
//! Expected values are hand-computed from openSCM `solver_spec.py`.

use std::collections::HashMap;

use scm_lp_optimizer::lp_model::{
    build_lp_model, compute_pair_cost, compute_supply_unit_costs, formulate_lp, var_idx, SLOT_I,
    SLOT_SS,
};
use scm_lp_optimizer::schema::{
    BomRow, FactRow, ItemLocationRow, LocationRow, OptimizeRequestPayload, ProductionLineRow,
    SolverConfig,
};

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

/// FG (cost 10, vmc 1) made from 2 x RAW (cost 4) with yield 0.5 and scrap 0.
fn request() -> OptimizeRequestPayload {
    let weeks = ["2026-01-05", "2026-01-12", "2026-01-19"];
    let mut facts = Vec::new();
    for w in weeks {
        facts.push(fact("PLANT", "FG", w, 10.0));
        facts.push(fact("PLANT", "RAW", w, 0.0));
    }
    OptimizeRequestPayload {
        config: SolverConfig::default(),
        facts,
        lines: vec![ProductionLineRow {
            location: "PLANT".to_string(),
            line_id: "L1".to_string(),
            max_capacity: 1000.0,
            available_hours_per_week: None,
        }],
        bom: vec![BomRow {
            target_location: "PLANT".to_string(),
            parent_sku: "FG".to_string(),
            child_sku: "RAW".to_string(),
            quantity: 2.0,
            line_id: Some("L1".to_string()),
            rate_per_hour: None,
            yield_rate: Some(0.5),
            scrap_rate: Some(0.0),
        }],
        item_locations: vec![
            ItemLocationRow {
                location: "PLANT".to_string(),
                sku: "FG".to_string(),
                cost: Some(30.0),
                variable_manufacturing_cost: Some(1.0),
                ..Default::default()
            },
            ItemLocationRow {
                location: "PLANT".to_string(),
                sku: "RAW".to_string(),
                cost: Some(4.0),
                ..Default::default()
            },
        ],
        locations: vec![LocationRow {
            location: "PLANT".to_string(),
            holding_cost: None,
            holding_cost_rate: Some(0.52),
            max_storage_capacity: None,
            allocation_priority: Some(1),
            storage_capacity_uom: None,
        }],
        ..Default::default()
    }
}

#[test]
fn bom_factor_includes_parent_yield() {
    let model = build_lp_model(&request()).unwrap().unwrap();
    let raw_p = model.pair_idx_map[&("PLANT".to_string(), "RAW".to_string())];
    let edges = &model.bom_consumers[&raw_p];
    // quantity / (1 - scrap) / yield = 2 / 1 / 0.5
    assert!((edges[0].1 - 4.0).abs() < 1e-12, "{edges:?}");
}

#[test]
fn value_added_cost_excludes_material() {
    let req = request();
    let model = build_lp_model(&req).unwrap().unwrap();
    // material = 4.0 factor * 4.0 cost = 16 -> va = 30 - 16 = 14
    let va = model.value_added_map[&("PLANT".to_string(), "FG".to_string())];
    assert!((va - 14.0).abs() < 1e-12, "va={va}");
    let fg = model.pair_idx_map[&("PLANT".to_string(), "FG".to_string())];
    let pc = compute_pair_cost(&model, fg, &req.config, 1);
    // prod = va + variable_manufacturing_cost
    assert!((pc.prod - 15.0).abs() < 1e-12, "prod={}", pc.prod);
    // holding_cost_rate is annual: 0.52 / 52 * 30 = 0.3
    assert!((pc.hc - 0.3).abs() < 1e-12, "hc={}", pc.hc);
    // allocation_priority == max_prio -> (1 + 0.05 * 1) x shortage 1000
    assert!((pc.shortage - 1050.0).abs() < 1e-9, "shortage={}", pc.shortage);
}

#[test]
fn terminal_inventory_and_ss_penalty_use_supply_unit_cost() {
    let req = request();
    let model = build_lp_model(&req).unwrap().unwrap();
    let max_prio = 1;
    let costs: Vec<_> = (0..model.n)
        .map(|p| compute_pair_cost(&model, p, &req.config, max_prio))
        .collect();
    let v = compute_supply_unit_costs(&model, &costs);
    let raw = model.pair_idx_map[&("PLANT".to_string(), "RAW".to_string())];
    let fg = model.pair_idx_map[&("PLANT".to_string(), "FG".to_string())];
    // RAW is procured at its standard cost 4; FG = prod 15 + 4.0 * v[RAW] = 31
    assert!((v[raw] - 4.0).abs() < 1e-12, "{v:?}");
    assert!((v[fg] - 31.0).abs() < 1e-12, "{v:?}");

    let lp = formulate_lp(&model, &req.config);
    let t = model.t_len;
    // terminal inventory: hc - 0.95 * v ; other weeks keep plain hc
    let expect_last = costs[fg].hc - 0.95 * v[fg];
    assert!((lp.c[var_idx(fg, t - 1, SLOT_I, t)] - expect_last).abs() < 1e-9);
    assert!((lp.c[var_idx(fg, 0, SLOT_I, t)] - costs[fg].hc).abs() < 1e-9);
    // SS penalty = max(100, min(1.0 * 31, 0.5 * 1050)) = 100
    assert!((lp.c[var_idx(fg, 0, SLOT_SS, t)] - 100.0).abs() < 1e-9);
}
