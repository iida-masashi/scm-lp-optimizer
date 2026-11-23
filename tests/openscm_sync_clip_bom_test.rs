//! BOM validity windows and firm-cell-aware post-processing clips.

use std::collections::HashMap;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{
    BomRow, FactRow, InitInventoryRow, ItemLocationRow, OptimizeRequestPayload, ProductionLineRow,
    TransportRouteRow,
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

fn qty(resp: &scm_lp_optimizer::schema::OptimizeResponsePayload, loc: &str, sku: &str, week: Option<&str>) -> i64 {
    resp.facts
        .iter()
        .filter(|f| f.location == loc && f.sku == sku && week.map_or(true, |w| f.week_start == w))
        .map(|f| f.replenishment_qty)
        .sum()
}

#[test]
fn bom_validity_window_selects_component_by_parent_start_week() {
    // FG needs 1 x RAW_A until 2026-01-12 and 1 x RAW_B from 2026-01-19 (production lead time 0).
    let mut req = base_request(0.0);
    req.facts.retain(|f| f.sku != "RAW");
    for f in req.facts.iter_mut() {
        if f.sku == "FG" && (f.week_start == WEEKS[0] || f.week_start == WEEKS[3]) {
            f.sales_qty = 10.0;
        }
    }
    for raw in ["RAW_A", "RAW_B"] {
        for w in WEEKS {
            req.facts.push(FactRow { sku: raw.to_string(), ..req.facts[0].clone() });
            let last = req.facts.last_mut().unwrap();
            last.week_start = w.to_string();
            last.sales_qty = 0.0;
        }
    }
    req.init_inventory.clear();
    let mk = |child: &str, vf: Option<&str>, vt: Option<&str>| BomRow {
        target_location: "PLANT".to_string(),
        parent_sku: "FG".to_string(),
        child_sku: child.to_string(),
        quantity: 1.0,
        line_id: Some("L1".to_string()),
        rate_per_hour: None,
        yield_rate: None,
        scrap_rate: None,
        valid_from: vf.map(|s| s.to_string()),
        valid_to: vt.map(|s| s.to_string()),
    };
    req.bom = vec![mk("RAW_A", None, Some("2026-01-12")), mk("RAW_B", Some("2026-01-19"), None)];
    req.item_locations = ["RAW_A", "RAW_B"]
        .iter()
        .map(|s| {
            let mut it = item("PLANT", s);
            it.can_procure = Some(true);
            it
        })
        .collect();
    let resp = run_optimization(&req).unwrap();
    let a = qty(&resp, "PLANT", "RAW_A", None);
    let b = qty(&resp, "PLANT", "RAW_B", None);
    assert!((a - 10).abs() <= 1 && (b - 10).abs() <= 1, "A={a} B={b}");
    assert_eq!(qty(&resp, "PLANT", "RAW_A", Some(WEEKS[3])), 0);
    assert_eq!(qty(&resp, "PLANT", "RAW_B", Some(WEEKS[0])), 0);
}

#[test]
fn firmed_shipment_survives_supply_clip_and_matches_transport_flows() {
    let mut req = base_request(0.0);
    for w in WEEKS {
        let mut f = fact("DC", "FG", w, 0.0);
        if w == WEEKS[1] {
            f.is_fixed = true;
            f.replenishment_qty = 30;
        }
        req.facts.push(f);
    }
    req.transport_routes = vec![TransportRouteRow {
        from_location: "PLANT".to_string(),
        to_location: "DC".to_string(),
        sku: "FG".to_string(),
        transport_lt: Some(0),
        transport_cost_per_unit: Some(1.0),
        min_shipping_qty: None,
        shipping_lot_size: Some(10),
        quota_pct: None,
    }];
    let resp = run_optimization(&req).unwrap();
    assert_eq!(qty(&resp, "DC", "FG", Some(WEEKS[1])), 30);
    let shipped: i64 = resp
        .transport_flows
        .iter()
        .filter(|r| r.ship_week == WEEKS[1])
        .map(|r| r.qty)
        .sum();
    assert_eq!(shipped, 30, "{:?}", resp.transport_flows);
}
