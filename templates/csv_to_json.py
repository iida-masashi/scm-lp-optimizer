#!/usr/bin/env python3
"""Convert a directory of CSV master and transaction templates into an OptimizeRequestPayload JSON."""

from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path
from typing import Any


def parse_opt_float(val: str | None) -> float | None:
    if val is None or val.strip() == "":
        return None
    return float(val.strip())


def parse_opt_int(val: str | None) -> int | None:
    if val is None or val.strip() == "":
        return None
    return int(float(val.strip()))


def parse_opt_bool(val: str | None) -> bool | None:
    if val is None or val.strip() == "":
        return None
    s = val.strip().lower()
    if s in ("true", "1", "yes", "y"):
        return True
    if s in ("false", "0", "no", "n"):
        return False
    return None


def read_csv_rows(path: Path) -> list[dict[str, str]]:
    if not path.exists():
        return []
    with path.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        return [row for row in reader if any(v.strip() for v in row.values() if v)]


def build_payload(csv_dir: Path) -> dict[str, Any]:
    config_path = csv_dir / "config.json"
    config = (
        json.loads(config_path.read_text(encoding="utf-8"))
        if config_path.exists()
        else {}
    )

    facts = []
    for r in read_csv_rows(csv_dir / "facts.csv"):
        facts.append(
            {
                "location": r["location"].strip(),
                "sku": r["sku"].strip(),
                "week_start": r["week_start"].strip(),
                "sales_qty": parse_opt_float(r.get("sales_qty")) or 0.0,
                "dependent_demand_qty": parse_opt_float(r.get("dependent_demand_qty")) or 0.0,
                "production_qty": parse_opt_int(r.get("production_qty")) or 0,
                "replenishment_qty": parse_opt_int(r.get("replenishment_qty")) or 0,
                "order_release_qty": parse_opt_int(r.get("order_release_qty")),
                "safety_stock_weeks": parse_opt_float(r.get("safety_stock_weeks")),
                "production_fence_weeks": parse_opt_int(r.get("production_fence_weeks")),
                "replenishment_fence_weeks": parse_opt_int(r.get("replenishment_fence_weeks")),
            }
        )

    lines = []
    for r in read_csv_rows(csv_dir / "lines.csv"):
        lines.append(
            {
                "location": r["location"].strip(),
                "line_id": r["line_id"].strip(),
                "max_capacity": float(r["max_capacity"].strip()),
                "available_hours_per_week": parse_opt_float(r.get("available_hours_per_week")),
            }
        )

    bom = []
    for r in read_csv_rows(csv_dir / "bom.csv"):
        line_id = r.get("line_id", "").strip()
        bom.append(
            {
                "target_location": r["target_location"].strip(),
                "parent_sku": r["parent_sku"].strip(),
                "child_sku": r["child_sku"].strip(),
                "quantity": float(r["quantity"].strip()),
                "line_id": line_id if line_id else None,
                "rate_per_hour": parse_opt_float(r.get("rate_per_hour")),
                "yield_rate": parse_opt_float(r.get("yield_rate")),
                "scrap_rate": parse_opt_float(r.get("scrap_rate")),
            }
        )

    transport_routes = []
    for r in read_csv_rows(csv_dir / "transport_routes.csv"):
        transport_routes.append(
            {
                "from_location": r["from_location"].strip(),
                "to_location": r["to_location"].strip(),
                "sku": r["sku"].strip(),
                "transport_lt": parse_opt_int(r.get("transport_lt")),
                "transport_cost_per_unit": parse_opt_float(r.get("transport_cost_per_unit")),
                "min_shipping_qty": parse_opt_int(r.get("min_shipping_qty")),
                "shipping_lot_size": parse_opt_int(r.get("shipping_lot_size")),
                "quota_pct": parse_opt_float(r.get("quota_pct")),
            }
        )

    item_locations = []
    for r in read_csv_rows(csv_dir / "item_locations.csv"):
        item_locations.append(
            {
                "location": r["location"].strip(),
                "sku": r["sku"].strip(),
                "can_procure": parse_opt_bool(r.get("can_procure")),
                "production_lt": parse_opt_int(r.get("production_lt")),
                "safety_lt_weeks": parse_opt_int(r.get("safety_lt_weeks")),
                "min_production_qty": parse_opt_int(r.get("min_production_qty")),
                "production_lot_size": parse_opt_int(r.get("production_lot_size")),
                "min_procurement_qty": parse_opt_int(r.get("min_procurement_qty")),
                "purchase_lot_size": parse_opt_int(r.get("purchase_lot_size")),
                "weekly_procurement_capacity": parse_opt_float(r.get("weekly_procurement_capacity")),
                "procurement_lt_weeks": parse_opt_int(r.get("procurement_lt_weeks")),
                "safety_stock_weeks": parse_opt_float(r.get("safety_stock_weeks")),
                "excess_inventory_buffer_ratio": parse_opt_float(
                    r.get("excess_inventory_buffer_ratio")
                ),
                "price": parse_opt_float(r.get("price")),
                "cost": parse_opt_float(r.get("cost")),
                "procurement_unit_cost": parse_opt_float(r.get("procurement_unit_cost")),
            }
        )

    locations = []
    for r in read_csv_rows(csv_dir / "locations.csv"):
        locations.append(
            {
                "location": r["location"].strip(),
                "holding_cost": parse_opt_float(r.get("holding_cost")),
                "holding_cost_rate": parse_opt_float(r.get("holding_cost_rate")),
                "max_storage_capacity": parse_opt_float(r.get("max_storage_capacity")),
            }
        )

    init_inventory = []
    for r in read_csv_rows(csv_dir / "init_inventory.csv"):
        init_inventory.append(
            {
                "location": r["location"].strip(),
                "sku": r["sku"].strip(),
                "qty": float(r["qty"].strip()),
            }
        )

    scheduled_receipts = []
    for r in read_csv_rows(csv_dir / "scheduled_receipts.csv"):
        scheduled_receipts.append(
            {
                "location": r["location"].strip(),
                "sku": r["sku"].strip(),
                "due_week": r["due_week"].strip(),
                "qty": float(r["qty"].strip()),
            }
        )

    capacity_overrides = []
    for r in read_csv_rows(csv_dir / "capacity_overrides.csv"):
        capacity_overrides.append(
            {
                "location": r["location"].strip(),
                "line_id": r["line_id"].strip(),
                "week_start": r["week_start"].strip(),
                "max_capacity": float(r["max_capacity"].strip()),
            }
        )

    return {
        "config": config,
        "facts": facts,
        "lines": lines,
        "bom": bom,
        "transport_routes": transport_routes,
        "item_locations": item_locations,
        "locations": locations,
        "init_inventory": init_inventory,
        "scheduled_receipts": scheduled_receipts,
        "backorder_skus": [],
        "capacity_overrides": capacity_overrides,
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Convert CSV master/transaction templates into OptimizeRequestPayload JSON."
    )
    parser.add_argument(
        "--csv-dir",
        type=Path,
        default=Path("templates/csv"),
        help="Directory containing CSV input files",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("request_from_csv.json"),
        help="Output JSON file path",
    )
    args = parser.parse_args()

    payload = build_payload(args.csv_dir)
    args.output.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"Wrote JSON request payload to: {args.output}")


if __name__ == "__main__":
    main()
