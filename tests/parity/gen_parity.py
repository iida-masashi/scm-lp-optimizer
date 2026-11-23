"""Generate openSCM-vs-Rust parity fixtures.

Run with the openSCM venv (needs highspy/polars):
    C:\\Users\\iidam\\gemini\\openSCM\\backend\\.venv\\Scripts\\python.exe tests/parity/gen_parity.py

For each scenario this writes `tests/parity/<name>.json` containing the Rust request payload
(`request`) and the plan produced by openSCM on the very same inputs (`expected`: per
(location, sku, week) production_qty / replenishment_qty). `tests/parity_test.rs` replays the
request through the Rust optimizer and compares the plans.
"""
import json
import sys
from datetime import date, timedelta
from pathlib import Path

OPENSCM = Path(r"C:\Users\iidam\gemini\openSCM\backend")
sys.path.insert(0, str(OPENSCM))
sys.path.insert(0, str(OPENSCM / "tests"))

import polars as pl  # noqa: E402
from solver import run_lp_optimization, run_milp_optimization  # noqa: E402
from _w1c_lp_fixtures import facts as mk_facts, lines as mk_lines, bom as mk_bom, il as mk_il, tr as mk_tr, inv as mk_inv, locs as mk_locs  # noqa: E402

OUT = Path(__file__).parent
W0 = date(2026, 1, 5)


def wk(T):
    return [(W0 + timedelta(weeks=i)).isoformat() for i in range(T)]


def scenario(name, T, fspec, lines=(), bom=(), il=(), tr=(), inv=(), locs=(), milp=False, dynamic=False,
             line_elig=None, firm=None):
    """firm: {(loc, sku): {"fixed": [...], "replen": [...], "prod": [...]}} merged into fspec."""
    spec = {k: dict(v) for k, v in fspec.items()}
    for k, v in (firm or {}).items():
        spec.setdefault(k, {}).update(v)
    f_df = mk_facts(spec, T)
    l_df = mk_lines(list(lines))
    b_df = mk_bom(list(bom))
    il_df = mk_il(list(il)) if il else None
    tr_df = mk_tr(list(tr)) if tr else None
    inv_df = mk_inv(list(inv))
    loc_df = mk_locs(list(locs)) if locs else None
    le_df = None
    if line_elig:
        le_df = pl.DataFrame(line_elig, schema_overrides={"efficiency_rate": pl.Float64})
    runner = run_milp_optimization if milp else run_lp_optimization
    kwargs = dict(item_locations_df=il_df, locations_df=loc_df, dynamic_sourcing=dynamic)
    if tr_df is not None:
        res, _ = runner(f_df, l_df, b_df, inv_df, tr_df, **kwargs)
    else:
        res, _ = runner(f_df, l_df, b_df, inv_df, **kwargs)
    if le_df is not None:
        res, _ = runner(f_df, l_df, b_df, inv_df, tr_df, line_eligibility_df=le_df, **kwargs)

    expected = []
    for r in res.sort(["location", "sku", "week_start"]).iter_rows(named=True):
        expected.append({
            "location": r["location"], "sku": r["sku"], "week_start": str(r["week_start"])[:10],
            "production_qty": int(r["production_qty"]), "replenishment_qty": int(r["replenishment_qty"]),
        })

    # ---- Rust request
    weeks = wk(T)
    facts = []
    for (loc, sku), s in spec.items():
        for t, w in enumerate(weeks):
            facts.append({
                "location": loc, "sku": sku, "week_start": w,
                "sales_qty": float(s.get("sales", [0] * T)[t]),
                "production_qty": int(s.get("prod", [0] * T)[t]),
                "replenishment_qty": int(s.get("replen", [0] * T)[t]),
                "is_fixed": bool(s.get("fixed", [0] * T)[t]),
                "production_fence_weeks": int(s.get("pf", 0)),
                "replenishment_fence_weeks": int(s.get("rf", 0)),
            })

    def clean(d):
        return {k: v for k, v in d.items() if v is not None}

    il_rows = []
    for r in il:
        m = {"location": r["location"], "sku": r["sku"]}
        for k_src, k_dst in [("price", "price"), ("cost", "cost"), ("production_lt", "production_lt"),
                             ("procurement_lt_weeks", "procurement_lt_weeks"),
                             ("min_production_qty", "min_production_qty"),
                             ("production_lot_size", "production_lot_size"),
                             ("min_procurement_qty", "min_procurement_qty"),
                             ("purchase_lot_size", "purchase_lot_size"),
                             ("can_procure", "can_procure"), ("procurement_unit_cost", "procurement_unit_cost"),
                             ("safety_stock_weeks", "safety_stock_weeks"), ("setup_cost", "setup_cost"),
                             ("setup_time_hours", "setup_time_hours")]:
            if k_src in r:
                m[k_dst] = r[k_src]
        il_rows.append(m)
    bom_rows = []
    for b in bom:
        d = dict(b) if isinstance(b, dict) else {"parent_sku": b[1 - 1], "child_sku": b[1], "quantity": b[2],
                                                 "target_location": b[3], "line_id": b[4] if len(b) > 4 else None}
        if not isinstance(b, dict):
            d = {"parent_sku": b[0], "child_sku": b[1], "quantity": b[2], "target_location": b[3],
                 "line_id": b[4] if len(b) > 4 else None}
            if len(b) > 5:
                d.update(b[5])
        bom_rows.append(clean({k: (str(v) if k in ("valid_from", "valid_to") and v is not None else v)
                               for k, v in d.items()}))
    tr_rows = []
    for r in tr:
        if isinstance(r, dict):
            d = r
        else:
            d = {"from_location": r[0], "to_location": r[1], "sku": r[2], "transport_lt": r[3],
                 "transport_cost_per_unit": r[4] if len(r) > 4 else None,
                 "shipping_lot_size": r[5] if len(r) > 5 else 1,
                 "min_shipping_qty": r[6] if len(r) > 6 else 0, "quota_pct": r[7] if len(r) > 7 else None}
        tr_rows.append(clean(d))
    line_rows = []
    for r in lines:
        line_rows.append(clean({"location": r[0], "line_id": r[1], "max_capacity": r[2],
                                "available_hours_per_week": r[3] if len(r) > 3 else None}))
    loc_rows = []
    for r in locs:
        loc_rows.append(clean(r if isinstance(r, dict) else {"location": r, "holding_cost": 1.0}))
    request = {
        "config": {"solver": "milp" if milp else "highs", "dynamic_sourcing": dynamic, "milp_relative_gap": 0.0},
        "facts": facts, "lines": line_rows, "bom": bom_rows, "transport_routes": tr_rows,
        "item_locations": il_rows, "locations": loc_rows,
        "init_inventory": [{"location": r[0], "sku": r[1], "qty": r[2]} for r in inv],
        "line_eligibility": [clean(r) for r in (line_elig or [])],
    }
    (OUT / f"{name}.json").write_text(json.dumps({"request": request, "expected": expected}, indent=1))
    print(name, "ok", sum(e["production_qty"] for e in expected), sum(e["replenishment_qty"] for e in expected))


T = 6
# 1. Two-echelon with BOM, line capacity, lots and safety stock
scenario(
    "p1_two_echelon_bom", T,
    {("PL", "FG"): {"sales": [0] * T}, ("PL", "RAW"): {"sales": [0] * T},
     ("DC", "FG"): {"sales": [30, 40, 50, 30, 40, 60]}},
    lines=[("PL", "L1", 80)],
    bom=[("FG", "RAW", 2, "PL", "L1")],
    il=[{"location": "PL", "sku": "RAW", "can_procure": True, "procurement_lt_weeks": 1, "procurement_unit_cost": 1.0,
         "purchase_lot_size": 20, "cost": 1.0},
        {"location": "PL", "sku": "FG", "cost": 10.0, "price": 40.0, "production_lot_size": 10, "production_lt": 1},
        {"location": "DC", "sku": "FG", "price": 40.0, "cost": 10.0, "safety_stock_weeks": 1.0}],
    tr=[("PL", "DC", "FG", 1, 1.0, 10, 0, None)],
    inv=[("PL", "RAW", 40.0), ("DC", "FG", 20.0)],
    locs=[{"location": "PL", "holding_cost": 0.5}, {"location": "DC", "holding_cost": 1.0}],
)

# 2. Capacity-constrained: demand spike, pre-build before the spike
scenario(
    "p2_capacity_prebuild", T,
    {("PL", "FG"): {"sales": [0] * T}, ("PL", "RAW"): {"sales": [0] * T},
     ("DC", "FG"): {"sales": [20, 20, 20, 150, 20, 20]}},
    lines=[("PL", "L1", 60)],
    bom=[("FG", "RAW", 1, "PL", "L1")],
    il=[{"location": "PL", "sku": "FG", "cost": 8.0, "price": 30.0},
        {"location": "DC", "sku": "FG", "price": 30.0, "cost": 8.0}],
    tr=[("PL", "DC", "FG", 1, 1.0)],
    inv=[("PL", "RAW", 100000.0)],
    locs=[{"location": "PL", "holding_cost": 0.2}, {"location": "DC", "holding_cost": 0.4}],
)

# 3. Yield/scrap BOM and value-added cost
scenario(
    "p3_yield_scrap_bom", T,
    {("PL", "FG"): {"sales": [25, 25, 25, 25, 25, 25]}, ("PL", "RAW"): {"sales": [0] * T}},
    lines=[("PL", "L1", 100)],
    bom=[{"parent_sku": "FG", "child_sku": "RAW", "quantity": 2.0, "scrap_rate": 0.1, "yield_rate": 0.8,
          "target_location": "PL", "line_id": "L1"}],
    il=[{"location": "PL", "sku": "RAW", "can_procure": True, "procurement_unit_cost": 3.0, "cost": 3.0},
        {"location": "PL", "sku": "FG", "cost": 12.0, "price": 50.0}],
    inv=[],
    locs=[{"location": "PL", "holding_cost": 0.5}],
)

# 4. Dynamic sourcing between two plants
scenario(
    "p4_dynamic_sourcing", T,
    {("P1", "FG"): {"sales": [0] * T}, ("P2", "FG"): {"sales": [0] * T}, ("DC", "FG"): {"sales": [50] * T},
     ("P1", "RAW"): {"sales": [0] * T}, ("P2", "RAW"): {"sales": [0] * T}},
    lines=[("P1", "L1", 60), ("P2", "L2", 60)],
    bom=[("FG", "RAW", 1, "P1", "L1"), ("FG", "RAW", 1, "P2", "L2")],
    il=[{"location": "P1", "sku": "FG", "cost": 5.0, "price": 30.0},
        {"location": "P2", "sku": "FG", "cost": 6.0, "price": 30.0},
        {"location": "DC", "sku": "FG", "cost": 5.0, "price": 30.0}],
    tr=[("P1", "DC", "FG", 1, 1.0), ("P2", "DC", "FG", 1, 2.0)],
    inv=[("P1", "RAW", 100000.0), ("P2", "RAW", 100000.0)],
    locs=[{"location": "P1", "holding_cost": 0.1}, {"location": "P2", "holding_cost": 0.1},
          {"location": "DC", "holding_cost": 0.1}],
    dynamic=True,
)

# 5. Firm cell
scenario(
    "p5_firm_cell", T,
    {("PL", "FG"): {"sales": [0] * T}, ("PL", "RAW"): {"sales": [0] * T}, ("DC", "FG"): {"sales": [30] * T}},
    lines=[("PL", "L1", 100)],
    bom=[("FG", "RAW", 1, "PL", "L1")],
    il=[{"location": "PL", "sku": "FG", "cost": 8.0, "price": 30.0},
        {"location": "DC", "sku": "FG", "price": 30.0, "cost": 8.0}],
    tr=[("PL", "DC", "FG", 1, 1.0)],
    inv=[("PL", "RAW", 100000.0)],
    locs=[{"location": "PL", "holding_cost": 0.2}, {"location": "DC", "holding_cost": 0.4}],
    firm={("PL", "FG"): {"fixed": [0, 1, 0, 0, 0, 0], "prod": [0, 70, 0, 0, 0, 0]}},
)

# 6. MILP: lots, minimum batch and setup cost
scenario(
    "p6_milp_lots_setup", 5,
    {("PL", "FG"): {"sales": [15, 0, 25, 0, 20]}, ("PL", "RAW"): {"sales": [0] * 5}},
    lines=[("PL", "L1", 200)],
    bom=[("FG", "RAW", 1, "PL", "L1")],
    il=[{"location": "PL", "sku": "FG", "cost": 8.0, "price": 30.0, "production_lot_size": 10,
         "min_production_qty": 30, "setup_cost": 400.0}],
    inv=[("PL", "RAW", 100000.0)],
    locs=[{"location": "PL", "holding_cost": 0.3}],
    milp=True,
)

# 7. BOM validity window
scenario(
    "p7_bom_window", 4,
    {("PL", "FG"): {"sales": [10, 0, 0, 10]}, ("PL", "RA"): {"sales": [0] * 4}, ("PL", "RB"): {"sales": [0] * 4}},
    lines=[("PL", "L1", 100)],
    bom=[("FG", "RA", 1, "PL", "L1", {"valid_to": "2026-01-12"}),
         ("FG", "RB", 1, "PL", "L1", {"valid_from": "2026-01-19"})],
    il=[{"location": "PL", "sku": "RA", "can_procure": True, "procurement_unit_cost": 1.0, "cost": 1.0},
        {"location": "PL", "sku": "RB", "can_procure": True, "procurement_unit_cost": 1.0, "cost": 1.0},
        {"location": "PL", "sku": "FG", "cost": 8.0, "price": 30.0}],
    inv=[],
    locs=[{"location": "PL", "holding_cost": 0.3}],
)
