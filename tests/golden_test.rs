use std::collections::HashMap;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::{
    BomRow, CapacityOverrideRow, FactRow, InitInventoryRow, ItemLocationRow, LocationRow,
    OptimizeRequestPayload, OptimizerError, ProductionLineRow, ScheduledReceiptRow, SolverConfig,
    TransportRouteRow,
};

fn make_facts(
    loc_skus: &[(&str, &str)],
    weeks: &[&str],
    demands: &[(&str, &str, &str, f64)],
) -> Vec<FactRow> {
    let mut out = Vec::new();
    for &(loc, sku) in loc_skus {
        for &w in weeks {
            let mut sales_qty = 0.0;
            for &(dl, ds, dw, dv) in demands {
                if dl == loc && ds == sku && dw == w {
                    sales_qty = dv;
                }
            }
            out.push(FactRow {
                location: loc.to_string(),
                sku: sku.to_string(),
                week_start: w.to_string(),
                sales_qty,
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
            });
        }
    }
    out
}

#[test]
fn test_two_echelon_bom_and_transport_lp() {
    let weeks = ["2026-01-05", "2026-01-12", "2026-01-19", "2026-01-26"];
    let pairs = [
        ("PLANT-A", "RAW-1"),
        ("PLANT-A", "FG-1"),
        ("DC-EAST", "FG-1"),
    ];
    let demands = [
        ("DC-EAST", "FG-1", "2026-01-12", 100.0),
        ("DC-EAST", "FG-1", "2026-01-19", 150.0),
        ("DC-EAST", "FG-1", "2026-01-26", 120.0),
    ];

    let req = OptimizeRequestPayload {
        config: SolverConfig::default(),
        facts: make_facts(&pairs, &weeks, &demands),
        lines: vec![ProductionLineRow {
            location: "PLANT-A".to_string(),
            line_id: "LINE-FG".to_string(),
            max_capacity: 200.0,
            available_hours_per_week: Some(120.0),
        }],
        bom: vec![BomRow {
            target_location: "PLANT-A".to_string(),
            parent_sku: "FG-1".to_string(),
            child_sku: "RAW-1".to_string(),
            quantity: 2.0,
            line_id: Some("LINE-FG".to_string()),
            rate_per_hour: Some(2.0),
            yield_rate: Some(1.0),
            scrap_rate: Some(0.0),
        }],
        init_inventory: vec![InitInventoryRow {
            location: "DC-EAST".to_string(),
            sku: "FG-1".to_string(),
            qty: 50.0,
        }],
        transport_routes: vec![TransportRouteRow {
            from_location: "PLANT-A".to_string(),
            to_location: "DC-EAST".to_string(),
            sku: "FG-1".to_string(),
            transport_lt: Some(1),
            transport_cost_per_unit: Some(0.5),
            min_shipping_qty: Some(10),
            shipping_lot_size: Some(10),
            quota_pct: None,
        }],
        item_locations: vec![
            ItemLocationRow {
                location: "PLANT-A".to_string(),
                sku: "RAW-1".to_string(),
                can_procure: Some(true),
                min_procurement_qty: Some(10),
                purchase_lot_size: Some(10),
                procurement_unit_cost: Some(1.0),
                ..Default::default()
            },
            ItemLocationRow {
                location: "PLANT-A".to_string(),
                sku: "FG-1".to_string(),
                min_production_qty: Some(10),
                production_lot_size: Some(10),
                cost: Some(2.0),
                ..Default::default()
            },
            ItemLocationRow {
                location: "DC-EAST".to_string(),
                sku: "FG-1".to_string(),
                safety_stock_weeks: Some(1.0),
                price: Some(20.0),
                ..Default::default()
            },
        ],
        locations: vec![
            LocationRow {
                location: "PLANT-A".to_string(),
                holding_cost: Some(0.5),
                holding_cost_rate: None,
                max_storage_capacity: Some(2000.0),
            },
            LocationRow {
                location: "DC-EAST".to_string(),
                holding_cost: Some(0.5),
                holding_cost_rate: None,
                max_storage_capacity: Some(1000.0),
            },
        ],
        scheduled_receipts: vec![ScheduledReceiptRow {
            location: "PLANT-A".to_string(),
            sku: "RAW-1".to_string(),
            due_week: "2026-01-05".to_string(),
            qty: 100.0,
        }],
        backorder_skus: Vec::new(),
        capacity_overrides: Vec::new(),
        request_map: Vec::new(),
        request_kind_map: Vec::new(),
    };

    let resp = run_optimization(&req).expect("Optimization should succeed");
    assert_eq!(resp.status, "optimal");
    assert!(!resp.edits.is_empty());

    // Total demand at DC-EAST is 100 + 150 + 120 = 370, with init_inv = 50 -> at least 320 shipped
    let dc_replen: i64 = resp
        .facts
        .iter()
        .filter(|f| f.location == "DC-EAST" && f.sku == "FG-1")
        .map(|f| f.replenishment_qty)
        .sum();
    assert!(
        dc_replen >= 320,
        "Expected DC-EAST total replenishment_qty >= 320, got {}",
        dc_replen
    );

    // PLANT-A must produce FG-1 to supply DC-EAST
    let plant_prod: i64 = resp
        .facts
        .iter()
        .filter(|f| f.location == "PLANT-A" && f.sku == "FG-1")
        .map(|f| f.production_qty)
        .sum();
    assert!(
        plant_prod >= 320,
        "Expected PLANT-A total FG-1 production_qty >= 320, got {}",
        plant_prod
    );
}

#[test]
fn test_capacity_bottleneck_shadow_price() {
    let weeks = ["2026-01-05", "2026-01-12"];
    let pairs = [("PLANT-A", "FG-1"), ("PLANT-A", "RAW-1")];
    let demands = [("PLANT-A", "FG-1", "2026-01-12", 250.0)];

    let req = OptimizeRequestPayload {
        config: SolverConfig::default(),
        facts: make_facts(&pairs, &weeks, &demands),
        lines: vec![ProductionLineRow {
            location: "PLANT-A".to_string(),
            line_id: "L1".to_string(),
            max_capacity: 100.0,
            available_hours_per_week: Some(100.0),
        }],
        bom: vec![BomRow {
            target_location: "PLANT-A".to_string(),
            parent_sku: "FG-1".to_string(),
            child_sku: "RAW-1".to_string(),
            quantity: 1.0,
            line_id: Some("L1".to_string()),
            rate_per_hour: None,
            yield_rate: Some(1.0),
            scrap_rate: Some(0.0),
        }],
        init_inventory: Vec::new(),
        transport_routes: Vec::new(),
        item_locations: vec![ItemLocationRow {
            location: "PLANT-A".to_string(),
            sku: "RAW-1".to_string(),
            can_procure: Some(true),
            ..Default::default()
        }],
        locations: Vec::new(),
        scheduled_receipts: Vec::new(),
        backorder_skus: Vec::new(),
        capacity_overrides: vec![CapacityOverrideRow {
            location: "PLANT-A".to_string(),
            line_id: "L1".to_string(),
            week_start: "2026-01-05".to_string(),
            max_capacity: 100.0,
        }],
        request_map: Vec::new(),
        request_kind_map: Vec::new(),
    };

    let resp = run_optimization(&req).expect("Optimization should succeed");
    assert_eq!(resp.status, "optimal");
    assert!(
        !resp.bottlenecks.is_empty(),
        "Expected binding capacity bottleneck shadow prices"
    );
}

#[test]
fn test_irregular_weekly_axis_rejected() {
    let weeks = ["2026-01-05", "2026-01-12", "2026-02-15"]; // 34-day jump
    let pairs = [("PLANT-A", "FG-1")];
    let demands = [("PLANT-A", "FG-1", "2026-01-12", 50.0)];

    let req = OptimizeRequestPayload {
        config: SolverConfig::default(),
        facts: make_facts(&pairs, &weeks, &demands),
        ..Default::default()
    };

    let err = run_optimization(&req).unwrap_err();
    match err {
        OptimizerError::SolverError { solver_message, .. } => {
            assert!(solver_message.contains("non-uniform planning axis"));
        }
        other => panic!("Expected SolverError for non-uniform axis, got {:?}", other),
    }
}

#[test]
fn test_example_and_template_files_valid() {
    let example_raw = std::fs::read_to_string("examples/basic_2echelon_request.json")
        .expect("Failed to read examples/basic_2echelon_request.json");
    let example_req: OptimizeRequestPayload =
        serde_json::from_str(&example_raw).expect("Failed to parse basic_2echelon_request.json");
    let example_resp = run_optimization(&example_req).expect("Example optimization failed");
    assert_eq!(example_resp.status, "optimal");
    assert!(!example_resp.edits.is_empty());
    assert!(!example_resp.line_utilizations.is_empty());
    assert!(!example_resp.transport_flows.is_empty());
    assert!(!example_resp.sku_psi_details.is_empty());

    let template_raw = std::fs::read_to_string("templates/request_template.json")
        .expect("Failed to read templates/request_template.json");
    let template_req: OptimizeRequestPayload =
        serde_json::from_str(&template_raw).expect("Failed to parse request_template.json");
    let template_resp = run_optimization(&template_req).expect("Template optimization failed");
    assert_eq!(template_resp.status, "optimal");
    assert!(!template_resp.line_utilizations.is_empty());
    assert!(!template_resp.sku_psi_details.is_empty());
}

#[test]
fn test_analytics_root_causes_and_natural_language_qa() {
    let weeks = ["2026-01-05", "2026-01-12", "2026-01-19"];
    let pairs = [
        ("PLANT-A", "RAW-1"),
        ("PLANT-A", "FG-1"),
        ("DC-EAST", "FG-1"),
    ];
    // Demand in 2026-01-19 (250) exceeds 1-week lagged PLANT-A line capacity (150) + init inv (20)
    let demands = [
        ("DC-EAST", "FG-1", "2026-01-12", 100.0),
        ("DC-EAST", "FG-1", "2026-01-19", 250.0),
    ];

    let req = OptimizeRequestPayload {
        config: SolverConfig::default(),
        facts: make_facts(&pairs, &weeks, &demands),
        lines: vec![ProductionLineRow {
            location: "PLANT-A".to_string(),
            line_id: "LINE-FG".to_string(),
            max_capacity: 150.0,
            available_hours_per_week: Some(75.0),
        }],
        bom: vec![BomRow {
            target_location: "PLANT-A".to_string(),
            parent_sku: "FG-1".to_string(),
            child_sku: "RAW-1".to_string(),
            quantity: 2.0,
            line_id: Some("LINE-FG".to_string()),
            rate_per_hour: Some(2.0),
            yield_rate: Some(1.0),
            scrap_rate: Some(0.0),
        }],
        init_inventory: vec![InitInventoryRow {
            location: "DC-EAST".to_string(),
            sku: "FG-1".to_string(),
            qty: 20.0,
        }],
        transport_routes: vec![TransportRouteRow {
            from_location: "PLANT-A".to_string(),
            to_location: "DC-EAST".to_string(),
            sku: "FG-1".to_string(),
            transport_lt: Some(1),
            transport_cost_per_unit: Some(0.5),
            min_shipping_qty: Some(10),
            shipping_lot_size: Some(10),
            quota_pct: None,
        }],
        item_locations: vec![
            ItemLocationRow {
                location: "PLANT-A".to_string(),
                sku: "RAW-1".to_string(),
                can_procure: Some(true),
                min_procurement_qty: Some(10),
                purchase_lot_size: Some(10),
                procurement_unit_cost: Some(1.0),
                ..Default::default()
            },
            ItemLocationRow {
                location: "PLANT-A".to_string(),
                sku: "FG-1".to_string(),
                min_production_qty: Some(10),
                production_lot_size: Some(10),
                cost: Some(2.0),
                ..Default::default()
            },
            ItemLocationRow {
                location: "DC-EAST".to_string(),
                sku: "FG-1".to_string(),
                safety_stock_weeks: Some(0.5),
                price: Some(20.0),
                ..Default::default()
            },
        ],
        locations: Vec::new(),
        scheduled_receipts: Vec::new(),
        backorder_skus: Vec::new(),
        capacity_overrides: Vec::new(),
        request_map: Vec::new(),
        request_kind_map: Vec::new(),
    };

    let resp = run_optimization(&req).expect("Optimization should succeed");
    assert_eq!(resp.status, "optimal");
    assert_eq!(resp.line_utilizations.len(), 3);
    assert!(!resp.transport_flows.is_empty());
    assert_eq!(resp.sku_psi_details.len(), 9);
    assert!(
        !resp.root_causes.is_empty(),
        "Expected upstream root causes for shortage"
    );

    let ask_req = scm_lp_optimizer::schema::AskRequestPayload {
        question: "なぜ欠品が発生するのか？原因と対策を教えて".to_string(),
        request: req,
        response: Some(resp),
        gemini_api_key: None,
    };
    let ask_resp = scm_lp_optimizer::qa::answer_question(&ask_req).expect("QA should succeed");
    assert!(ask_resp.answer_markdown.contains("DC-EAST"));
    assert!(ask_resp.answer_markdown.contains("LINE-FG"));
    assert!(!ask_resp.suggested_actions.is_empty());
}


