use crate::schema::{
    BomRow, CapacityOverrideRow, InitInventoryRow, ItemLocationRow, LineEligibilityRow,
    LocationRow, ProductionLineRow, TransportRouteRow, UomFactorRow,
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
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
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
    /// `(target_location, parent_sku)` -> normalized yield rate (default 1.0).
    pub yield_map: HashMap<LocSku, f64>,
    pub var_mfg_cost_map: HashMap<LocSku, f64>,
    pub allocation_priority_map: HashMap<String, i64>,
    /// `location -> storage UOM` (upper-case; BASE omitted).
    pub storage_uom_map: HashMap<String, String>,
    pub uom_factors: HashMap<(String, String), f64>,
    /// `(location, sku) -> [(alt_line, efficiency)]` sorted by (-efficiency, line_id).
    pub line_eligibility_map: HashMap<LocSku, Vec<(String, f64)>>,
    pub min_rem_shelf_map: HashMap<LocSku, i64>,
    pub setup_cost_map: HashMap<LocSku, f64>,
    pub setup_time_map: HashMap<LocSku, f64>,
}

/// Parent process yield as a fraction in (0, 1]. None / <=0 / >1 read as 1.0.
pub fn normalize_yield_rate(y: Option<f64>) -> f64 {
    match y {
        Some(v) if v > 0.0 && v <= 1.0 => v,
        _ => 1.0,
    }
}

/// Component scrap as a fraction in [0, 1). None / <0 / >=1 read as 0.0.
pub fn normalize_scrap_rate(s: Option<f64>) -> f64 {
    match s {
        Some(v) if (0.0..1.0).contains(&v) => v,
        _ => 0.0,
    }
}

/// Round qty up to respect MOQ and lot_size constraints (`solver_base._apply_lot_sizing`).
pub fn apply_lot_sizing(qty_f64: f64, moq: i64, lot_size: i64) -> i64 {
    if qty_f64 <= 0.0 {
        return 0;
    }
    let moq_f = moq.max(0) as f64;
    let qty = qty_f64.max(moq_f);
    // The interior-point LP solution carries ~1e-8 relative noise (10.0000003 must not become 11);
    // openSCM's simplex needs only 1e-9, so a slightly larger relative tolerance is used here.
    let tol = 1e-6 * qty.max(1.0);
    if lot_size > 1 {
        let lot_f = lot_size as f64;
        (((qty - tol) / lot_f).ceil().max(1.0) as i64) * lot_size
    } else {
        ((qty - tol).ceil().max(1.0)) as i64
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
    line_eligibility: &[LineEligibilityRow],
    uom_factors: &[UomFactorRow],
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
        lookups
            .yield_map
            .entry(rate_key.clone())
            .or_insert(normalize_yield_rate(r.yield_rate));
        if !lookups.sku_production_rate.contains_key(&rate_key) {
            let rph = r.rate_per_hour.unwrap_or(0.0);
            let yr = normalize_yield_rate(r.yield_rate);
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
            scrap_rate: normalize_scrap_rate(r.scrap_rate),
            line_id: r.line_id.clone(),
            valid_from: r.valid_from.clone(),
            valid_to: r.valid_to.clone(),
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
        if let Some(sc) = r.setup_cost {
            if sc > 0.0 {
                lookups.setup_cost_map.insert(key.clone(), sc);
            }
        }
        if let Some(st) = r.setup_time_hours {
            if st > 0.0 {
                lookups.setup_time_map.insert(key.clone(), st);
            }
        }
        if let Some(mr) = r.min_remaining_shelf_life_weeks {
            if mr > 0 {
                lookups.min_rem_shelf_map.insert(key.clone(), mr);
            }
        }
        if let Some(vmc) = r.variable_manufacturing_cost {
            if vmc > 0.0 {
                lookups.var_mfg_cost_map.insert(key.clone(), vmc);
            }
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
        if let Some(u) = row.storage_capacity_uom.as_deref() {
            let u = u.trim().to_uppercase();
            if !u.is_empty() && u != "BASE" {
                lookups.storage_uom_map.insert(row.location.clone(), u);
            }
        }
        if let Some(ap) = row.allocation_priority {
            lookups.allocation_priority_map.insert(row.location.clone(), ap);
        }
        if let Some(sc) = row.max_storage_capacity {
            lookups.storage_capacity_map.insert(row.location.clone(), sc);
        }
    }

    for u in uom_factors {
        lookups
            .uom_factors
            .insert((u.sku.clone(), u.uom.trim().to_uppercase()), u.factor);
    }

    for e in line_eligibility {
        let eff = match e.efficiency_rate {
            Some(x) if x != 0.0 => x,
            _ => 1.0,
        };
        if eff > 0.0 && lookups.line_capacity.contains_key(&(e.location.clone(), e.line_id.clone())) {
            lookups
                .line_eligibility_map
                .entry((e.location.clone(), e.sku.clone()))
                .or_default()
                .push((e.line_id.clone(), eff));
        }
    }
    for v in lookups.line_eligibility_map.values_mut() {
        v.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
    }

    lookups
}

/// `storage_uom_coeff`: inventory-to-storage-UOM multiplier for a SKU.
pub fn storage_uom_coeff(
    sku: &str,
    suom: Option<&String>,
    uom_factors: &HashMap<(String, String), f64>,
) -> f64 {
    if let Some(u) = suom {
        if let Some(&fac) = uom_factors.get(&(sku.to_string(), u.clone())) {
            if fac > 0.0 {
                return 1.0 / fac;
            }
        }
    }
    1.0
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
