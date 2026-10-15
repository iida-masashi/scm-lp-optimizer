use crate::schema::{
    BomRow, CapacityOverrideRow, InitInventoryRow, ItemLocationRow, LocationRow,
    ProductionLineRow, TransportRouteRow,
};
use std::collections::{HashMap, HashSet};

pub type LocSku = (String, String);
pub type LocLine = (String, String);

#[derive(Debug, Clone)]
pub struct RateInfo {
    pub rate_per_hour: f64,
    pub yield_rate: f64,
}

#[derive(Debug, Clone)]
pub struct EnrichedBomRow {
    pub source_location: String,
    pub target_location: String,
    pub parent_sku: String,
    pub child_sku: String,
    pub quantity: f64,
    pub scrap_rate: f64,
    pub line_id: Option<String>,
}

/// Master-data lookup dictionaries (Rust equivalent of `solver_base._build_master_lookups`).
#[derive(Debug, Clone, Default)]
pub struct MasterLookups {
    pub line_capacity: HashMap<LocLine, f64>,
    pub line_hours: HashMap<LocLine, f64>,
    pub location_capacity: HashMap<String, f64>,
    pub sku_to_line: HashMap<LocSku, String>,
    pub sku_production_rate: HashMap<LocSku, RateInfo>,
    pub bom_keys: HashSet<LocSku>,
    pub bom_list: Vec<EnrichedBomRow>,
    pub transport_keys: HashSet<LocSku>,
    pub transport_list: Vec<TransportRouteRow>,
    pub transport_cost: HashMap<LocSku, f64>,
    pub prod_lt_map: HashMap<LocSku, i64>,
    pub prod_block_lt_map: HashMap<LocSku, i64>,
    pub production_lot_map: HashMap<LocSku, (i64, i64)>,
    pub procurement_lot_map: HashMap<LocSku, (i64, i64)>,
    pub procurement_capacity_map: HashMap<LocSku, f64>,
    pub procurement_lt_map: HashMap<LocSku, i64>,
    pub shipping_lot_map: HashMap<LocSku, (i64, i64)>,
    pub ss_weeks_map: HashMap<LocSku, f64>,
    pub buffer_ratio_map: HashMap<LocSku, f64>,
    pub init_inv_map: HashMap<LocSku, f64>,
    pub location_holding_cost: HashMap<String, f64>,
    pub location_holding_rate: HashMap<String, f64>,
    pub storage_capacity_map: HashMap<String, f64>,
    pub margin_map: HashMap<LocSku, f64>,
    pub price_map: HashMap<LocSku, f64>,
    pub cost_map: HashMap<LocSku, f64>,
    pub procurement_cost_map: HashMap<LocSku, f64>,
    pub can_procure_override_map: HashMap<LocSku, bool>,
}

/// Round qty up to respect MOQ and lot_size constraints (`solver_base._apply_lot_sizing`).
pub fn apply_lot_sizing(qty_f64: f64, moq: i64, lot_size: i64) -> i64 {
    if qty_f64 <= 0.0 {
        return 0;
    }
    let moq_f = moq.max(0) as f64;
    let qty = qty_f64.max(moq_f);
    if lot_size > 1 {
        let lot_f = lot_size as f64;
        ((qty / lot_f).ceil() as i64) * lot_size
    } else {
        qty.ceil() as i64
    }
}

/// Standard MRP consume-at-destination model (`planning_helpers.resolve_bom_source_location`).
pub fn resolve_bom_source_location(target_location: &str) -> String {
    target_location.to_string()
}

pub fn build_master_lookups(
    lines: &[ProductionLineRow],
    bom: &[BomRow],
    transport: &[TransportRouteRow],
    item_locations: &[ItemLocationRow],
    init_inv: &[InitInventoryRow],
    locations: &[LocationRow],
) -> MasterLookups {
    let mut lookups = MasterLookups::default();

    for row in lines {
        let key = (row.location.clone(), row.line_id.clone());
        let cap = row.max_capacity;
        let hours = match row.available_hours_per_week {
            Some(h) if h > 0.0 => h,
            _ => 120.0,
        };
        lookups.line_capacity.insert(key.clone(), cap);
        lookups.line_hours.insert(key, hours);
        *lookups
            .location_capacity
            .entry(row.location.clone())
            .or_insert(0.0) += cap;
    }

    for r in bom {
        let rate_key = (r.target_location.clone(), r.parent_sku.clone());
        lookups.bom_keys.insert(rate_key.clone());
        if let Some(ref line_id) = r.line_id {
            if !line_id.is_empty() {
                lookups.sku_to_line.insert(rate_key.clone(), line_id.clone());
            }
        }
        if !lookups.sku_production_rate.contains_key(&rate_key) {
            let rph = r.rate_per_hour.unwrap_or(0.0);
            let yr = match r.yield_rate {
                Some(y) if y > 0.0 => y,
                _ => 1.0,
            };
            if rph > 0.0 {
                lookups.sku_production_rate.insert(
                    rate_key,
                    RateInfo {
                        rate_per_hour: rph,
                        yield_rate: yr,
                    },
                );
            }
        }
        lookups.bom_list.push(EnrichedBomRow {
            source_location: resolve_bom_source_location(&r.target_location),
            target_location: r.target_location.clone(),
            parent_sku: r.parent_sku.clone(),
            child_sku: r.child_sku.clone(),
            quantity: r.quantity,
            scrap_rate: r.scrap_rate.unwrap_or(0.0),
            line_id: r.line_id.clone(),
        });
    }

    for r in transport {
        let key = (r.to_location.clone(), r.sku.clone());
        lookups.transport_keys.insert(key.clone());
        lookups.transport_list.push(r.clone());
        let cost = r.transport_cost_per_unit.unwrap_or(0.0);
        if cost > 0.0 {
            let entry = lookups.transport_cost.entry(key.clone()).or_insert(cost);
            if cost < *entry {
                *entry = cost;
            }
        }
        let ship_min = r.min_shipping_qty.unwrap_or(0);
        let ship_lot = r.shipping_lot_size.unwrap_or(1);
        if ship_min > 0 || ship_lot > 1 {
            lookups.shipping_lot_map.insert(key, (ship_min, ship_lot));
        }
    }

    for r in item_locations {
        let key = (r.location.clone(), r.sku.clone());
        if let Some(cp) = r.can_procure {
            lookups.can_procure_override_map.insert(key.clone(), cp);
        }
        if let Some(plt) = r.production_lt {
            if plt != 0 {
                lookups.prod_lt_map.insert(key.clone(), plt);
                let safety_lt = r.safety_lt_weeks.unwrap_or(0);
                lookups
                    .prod_block_lt_map
                    .insert(key.clone(), (plt - safety_lt).max(0));
            }
        }
        let p_min = r.min_production_qty.unwrap_or(0);
        let p_lot = r.production_lot_size.unwrap_or(1);
        if p_min > 0 || p_lot > 1 {
            lookups.production_lot_map.insert(key.clone(), (p_min, p_lot));
        }
        let pr_min = r.min_procurement_qty.unwrap_or(0);
        let pr_lot = r.purchase_lot_size.unwrap_or(1);
        if pr_min > 0 || pr_lot > 1 {
            lookups.procurement_lot_map.insert(key.clone(), (pr_min, pr_lot));
        }
        if let Some(pcap) = r.weekly_procurement_capacity {
            lookups.procurement_capacity_map.insert(key.clone(), pcap);
        }
        if let Some(e_lt) = r.procurement_lt_weeks {
            if e_lt > 0 {
                lookups.procurement_lt_map.insert(key.clone(), e_lt);
            }
        }
        if let Some(ss) = r.safety_stock_weeks {
            lookups.ss_weeks_map.insert(key.clone(), ss);
        }
        if let Some(bratio) = r.excess_inventory_buffer_ratio {
            lookups.buffer_ratio_map.insert(key.clone(), bratio);
        }
        if let Some(price) = r.price {
            lookups.price_map.insert(key.clone(), price);
            let cost = r.cost.unwrap_or(0.0);
            lookups.margin_map.insert(key.clone(), price - cost);
        }
        if let Some(cost) = r.cost {
            lookups.cost_map.insert(key.clone(), cost);
        }
        if let Some(proc_cost) = r.procurement_unit_cost {
            lookups.procurement_cost_map.insert(key, proc_cost);
        }
    }

    for row in init_inv {
        lookups
            .init_inv_map
            .insert((row.location.clone(), row.sku.clone()), row.qty);
    }

    for row in locations {
        if let Some(hc) = row.holding_cost {
            lookups.location_holding_cost.insert(row.location.clone(), hc);
        }
        if let Some(hr) = row.holding_cost_rate {
            lookups.location_holding_rate.insert(row.location.clone(), hr);
        }
        if let Some(sc) = row.max_storage_capacity {
            lookups.storage_capacity_map.insert(row.location.clone(), sc);
        }
    }

    lookups
}

#[derive(Debug, Clone, Default)]
pub struct CapacityCalendar {
    pub line_cap_t: HashMap<(String, String, usize), f64>,
    pub loc_cap_t: HashMap<(String, usize), f64>,
    pub hours_t: HashMap<(String, String, usize), f64>,
}

/// Turn week-keyed capacity overrides into t-indexed maps (`solver_base._build_capacity_calendar`).
pub fn build_capacity_calendar(
    line_capacity: &HashMap<LocLine, f64>,
    location_capacity: &HashMap<String, f64>,
    line_hours: &HashMap<LocLine, f64>,
    capacity_overrides: &[CapacityOverrideRow],
    weeks: &[String],
) -> CapacityCalendar {
    let mut cal = CapacityCalendar::default();
    if capacity_overrides.is_empty() {
        return cal;
    }
    let week_idx: HashMap<&str, usize> = weeks
        .iter()
        .enumerate()
        .map(|(t, w)| (w.as_str(), t))
        .collect();
    let mut loc_delta: HashMap<(String, usize), f64> = HashMap::new();

    for ov in capacity_overrides {
        let Some(&t) = week_idx.get(ov.week_start.as_str()) else {
            continue;
        };
        let cap = ov.max_capacity;
        let line_key = (ov.location.clone(), ov.line_id.clone());
        cal.line_cap_t
            .insert((ov.location.clone(), ov.line_id.clone(), t), cap);
        let static_cap = *line_capacity.get(&line_key).unwrap_or(&0.0);
        *loc_delta.entry((ov.location.clone(), t)).or_insert(0.0) += cap - static_cap;
        let hours = *line_hours.get(&line_key).unwrap_or(&0.0);
        if hours > 0.0 {
            let scaled = if static_cap > 0.0 {
                hours * cap / static_cap
            } else {
                hours
            };
            cal.hours_t
                .insert((ov.location.clone(), ov.line_id.clone(), t), scaled);
        }
    }

    for ((loc, t), d) in loc_delta {
        let base = *location_capacity.get(&loc).unwrap_or(&0.0);
        cal.loc_cap_t.insert((loc, t), (base + d).max(0.0));
    }

    cal
}
