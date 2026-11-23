#![cfg(feature = "highs")]
//! Numeric parity with openSCM: each fixture in `tests/parity/` holds a request and the plan openSCM
//! (HiGHS) produced on the same inputs (regenerate with `tests/parity/gen_parity.py`). The Rust
//! optimizer, solved with `solver = "highs"` / `"milp"`, must reproduce production and replenishment
//! cell for cell.

use std::collections::HashMap;
use std::fs;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::OptimizeRequestPayload;

fn check(name: &str) {
    let path = format!("{}/tests/parity/{}.json", env!("CARGO_MANIFEST_DIR"), name);
    let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let req: OptimizeRequestPayload = serde_json::from_value(doc["request"].clone()).unwrap();
    let resp = run_optimization(&req).unwrap();
    let got: HashMap<(String, String, String), (i64, i64)> = resp
        .facts
        .iter()
        .map(|f| {
            (
                (f.location.clone(), f.sku.clone(), f.week_start[..10].to_string()),
                (f.production_qty, f.replenishment_qty),
            )
        })
        .collect();
    let mut diffs = Vec::new();
    for e in doc["expected"].as_array().unwrap() {
        let key = (
            e["location"].as_str().unwrap().to_string(),
            e["sku"].as_str().unwrap().to_string(),
            e["week_start"].as_str().unwrap().to_string(),
        );
        let want = (e["production_qty"].as_i64().unwrap(), e["replenishment_qty"].as_i64().unwrap());
        if got.get(&key) != Some(&want) {
            let g = got.get(&key).copied();
            diffs.push((key, want, g));
        }
    }
    assert!(diffs.is_empty(), "{name}: {} cells differ from openSCM, e.g. {:?}", diffs.len(), &diffs[..diffs.len().min(5)]);
}

#[test]
fn parity_two_echelon_bom() { check("p1_two_echelon_bom"); }
#[test]
fn parity_capacity_prebuild() { check("p2_capacity_prebuild"); }
#[test]
fn parity_yield_scrap_bom() { check("p3_yield_scrap_bom"); }
#[test]
fn parity_dynamic_sourcing() { check("p4_dynamic_sourcing"); }
#[test]
fn parity_firm_cell() { check("p5_firm_cell"); }
#[test]
fn parity_milp_lots_setup() { check("p6_milp_lots_setup"); }
#[test]
fn parity_bom_window() { check("p7_bom_window"); }
