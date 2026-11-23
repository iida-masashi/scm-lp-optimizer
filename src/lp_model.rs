use crate::graph::{
    assert_uniform_weekly_axis, bucket_receipts_by_week, compute_safety_stock_target,
    propagate_downstream_demand, supplier_to_consumer_order, BomWindows,
};
use crate::lookups::{build_capacity_calendar, build_master_lookups, CapacityCalendar, MasterLookups};
use crate::schema::{
    FactRow, ModelWarning, OptimizeRequestPayload, OptimizerError, SolverConfig,
};
use chrono::{Duration, NaiveDate};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub const V_SLOTS: usize = 6;
pub const SLOT_P: usize = 0;
pub const SLOT_R: usize = 1;
pub const SLOT_I: usize = 2;
pub const SLOT_U: usize = 3;
pub const SLOT_E: usize = 4;
pub const SLOT_SS: usize = 5;

#[inline]
pub fn var_idx(p: usize, t: usize, v: usize, t_len: usize) -> usize {
    p * t_len * V_SLOTS + t * V_SLOTS + v
}

/// Tuple describing a `(location, sku)` planning node (`all_pairs[p]`).
#[derive(Debug, Clone)]
pub struct PairNode {
    pub location: String,
    pub sku: String,
    pub can_produce: bool,
    pub can_replenish: bool,
    pub can_procure: bool,
}

/// Assembled backend-agnostic LP model (`solver_lp_model._build_lp_model`).
#[derive(Debug, Clone)]
pub struct LpModel {
    pub lookups: MasterLookups,
    pub all_pairs: Vec<PairNode>,
    pub pair_idx_map: HashMap<(String, String), usize>,
    pub n: usize,
    pub t_len: usize,
    pub weeks: Vec<String>,
    pub facts_lookup: HashMap<(String, String, String), FactRow>,
    pub demand: Vec<Vec<f64>>,
    pub target_inv: Vec<Vec<f64>>,
    pub receipts: Option<Vec<Vec<f64>>>,
    pub calendar: CapacityCalendar,
    pub backorder_pairs: HashSet<usize>,
    /// `supplier_p -> Vec<(parent_p, effective_bom_qty, prod_lt)>`
    pub bom_consumers: HashMap<usize, Vec<(usize, f64, i64)>>,
    /// Validity windows parallel to `bom_consumers`.
    pub bom_windows: BomWindows,
    /// `supplier_p -> Vec<(dst_p, trans_lt, mult)>`
    pub transport_consumers: HashMap<usize, Vec<(usize, i64, f64)>>,
    /// `Vec<(src_p, dst_p, trans_lt, cost)>` for Network Design formulation
    pub transport_edges: Vec<(usize, usize, i64, f64)>,
    pub has_quota_split: bool,
    pub supply_p_lt: HashMap<usize, i64>,
    pub supply_r_lt: HashMap<usize, i64>,
    /// `dst_p -> sum(mult * route_unit_cost)` over active inbound routes (openSCM `replen_cost_by_dst`).
    pub replen_cost_by_dst: HashMap<usize, f64>,
    /// `(location, parent_sku) -> max(0, standard_cost - material_cost)` (value-added production cost).
    pub value_added_map: HashMap<(String, String), f64>,
    /// Supplier-only pairs absent from `facts` (appended after the facts pairs; no P/R/E).
    pub ghost_pairs: HashSet<usize>,
    /// Projected FEFO expiry of initial lots per `(pair, week)` (added to the balance RHS).
    pub projected_expiries: Option<Vec<Vec<f64>>>,
    /// Alternate-line spillover edges.
    pub alt_line_edges: Vec<AltLineEdge>,
    /// All routes of destinations that use dynamic sourcing (subset of `transport_edges`).
    pub dynamic_transport_edges: Vec<(usize, usize, i64, f64)>,
    /// Firm purchase arrival `A` per protected replenishment cell `(pair, week)`.
    pub firm_purchase: HashMap<(usize, usize), f64>,
    /// Firm purchases whose order week falls before the horizon (subtracted from the balance RHS).
    pub firm_purchase_rhs: HashMap<(usize, usize), f64>,
    pub protected_p: HashSet<(usize, usize)>,
    pub protected_r: HashSet<(usize, usize)>,
    pub protected_e: HashSet<(usize, usize)>,
    pub has_firm_cells: bool,
    /// Retry mode after infeasibility: protected cells become `[0, qty]` with a large reward.
    pub relax_firm: bool,
    pub warnings: Vec<ModelWarning>,
}

/// Alternate production line edge (`alt_line_edges` tuple in openSCM).
#[derive(Debug, Clone)]
pub struct AltLineEdge {
    pub p: usize,
    pub location: String,
    pub sku: String,
    pub alt_line: String,
    pub efficiency: f64,
    pub alt_cost: f64,
}

/// `_cell_protected`: firmed bucket (`is_fixed`) or inside the time fence.
pub fn cell_protected(row: Option<&FactRow>, t: usize, replenishment: bool) -> bool {
    let Some(r) = row else { return false };
    if r.is_fixed {
        return true;
    }
    let fence = if replenishment {
        r.replenishment_fence_weeks
    } else {
        r.production_fence_weeks
    };
    (t as i64) < fence.unwrap_or(0)
}

/// `compute_projected_lot_expiries`: FEFO simulation of initial lots against demand.
/// Returns `week index -> expired qty`.
pub fn compute_projected_lot_expiries(
    lots: &[(f64, NaiveDate)],
    demand: &[f64],
    weeks: &[NaiveDate],
    min_remaining_weeks: i64,
) -> Vec<(usize, f64)> {
    let mut pool: Vec<(NaiveDate, f64)> = lots.iter().map(|&(q, d)| (d, q)).collect();
    pool.sort_by_key(|&(d, _)| d);
    let mut out = Vec::new();
    for (t, w) in weeks.iter().enumerate() {
        let cutoff = *w + Duration::weeks(min_remaining_weeks.max(0));
        let mut expired = 0.0;
        for item in pool.iter_mut() {
            if item.1 > 0.0 && item.0 <= cutoff {
                expired += item.1;
                item.1 = 0.0;
            }
        }
        if expired > 0.0 {
            out.push((t, expired));
        }
        let mut dem = demand.get(t).copied().unwrap_or(0.0);
        for item in pool.iter_mut() {
            if dem <= 0.0 {
                break;
            }
            if item.1 <= 0.0 {
                continue;
            }
            let take = item.1.min(dem);
            item.1 -= take;
            dem -= take;
        }
        if pool.iter().all(|i| i.1 <= 0.0) {
            break;
        }
    }
    out
}

/// `route_unit_cost`: positive route cost, else `config.replenishment_cost`.
pub fn route_unit_cost(route_cost: Option<f64>, cfg: &SolverConfig) -> f64 {
    match route_cost {
        Some(c) if c > 0.0 => c,
        _ => cfg.replenishment_cost,
    }
}

/// Per-pair objective coefficients (`compute_pair_cost_coeffs`).
#[derive(Debug, Clone, Copy)]
pub struct PairCost {
    pub hc: f64,
    pub prod: f64,
    pub replen: f64,
    pub procure: f64,
    pub shortage: f64,
}

fn bom_factor(quantity: f64, scrap: f64, yield_rate: f64) -> f64 {
    let s = if scrap < 1.0 { 1.0 - scrap } else { 1.0 };
    quantity / s / yield_rate
}

/// `compute_value_added_costs`: parent standard cost minus yield/scrap-adjusted child material cost.
/// Only parents that have a standard cost and BOM rows are returned; negatives clamp to 0.
pub fn compute_value_added_costs(lookups: &MasterLookups) -> HashMap<(String, String), f64> {
    let mut mat: HashMap<(String, String), f64> = HashMap::new();
    for r in &lookups.bom_list {
        let parent = (r.target_location.clone(), r.parent_sku.clone());
        if !lookups.cost_map.contains_key(&parent) {
            continue;
        }
        let yr = *lookups.yield_map.get(&parent).unwrap_or(&1.0);
        let factor = bom_factor(r.quantity, r.scrap_rate, yr);
        let child = (r.target_location.clone(), r.child_sku.clone());
        let child_cost = lookups
            .cost_map
            .get(&child)
            .or_else(|| lookups.procurement_cost_map.get(&child))
            .copied()
            .unwrap_or(0.0);
        *mat.entry(parent).or_insert(0.0) += factor * child_cost;
    }
    mat.into_iter()
        .map(|(k, m)| {
            let va = lookups.cost_map[&k] - m;
            (k, va.max(0.0))
        })
        .collect()
}

/// `compute_pair_cost_coeffs` for one `(location, sku)` pair.
pub fn compute_pair_cost(model: &LpModel, p: usize, cfg: &SolverConfig, max_prio: i64) -> PairCost {
    let pair = &model.all_pairs[p];
    let key = (pair.location.clone(), pair.sku.clone());
    let replen = model
        .replen_cost_by_dst
        .get(&p)
        .copied()
        .unwrap_or(cfg.replenishment_cost);
    let sku_cost = model.lookups.cost_map.get(&key).copied();
    let hr = model.lookups.location_holding_rate.get(&pair.location).copied();
    let hc = match (hr, sku_cost) {
        // `holding_cost_rate` is an ANNUAL rate (openSCM: rate / 52 per week).
        (Some(rate), Some(sc)) => (rate / 52.0) * sc,
        _ => *model
            .lookups
            .location_holding_cost
            .get(&pair.location)
            .unwrap_or(&cfg.holding_cost),
    };
    let vmc = model.lookups.var_mfg_cost_map.get(&key).copied().unwrap_or(0.0);
    let prod = match model.value_added_map.get(&key) {
        Some(va) => *va + vmc,
        None => sku_cost.unwrap_or(cfg.production_cost) + vmc,
    };
    let procure = model
        .lookups
        .procurement_cost_map
        .get(&key)
        .copied()
        .or(sku_cost)
        .unwrap_or(cfg.procurement_cost);
    let sku_price = model.lookups.price_map.get(&key).copied();
    let mut shortage = match sku_price {
        Some(sp) if cfg.profit_maximization => sp,
        Some(sp) => sp.max(cfg.shortage_penalty),
        None => cfg.shortage_penalty,
    };
    if let Some(&ap) = model.lookups.allocation_priority_map.get(&pair.location) {
        if max_prio > 0 {
            shortage *= 1.0 + 0.05 * ((max_prio - ap + 1).max(0) as f64);
        }
    }
    PairCost { hc, prod, replen, procure, shortage }
}

/// `compute_supply_unit_costs`: cheapest unit cost to make/ship/buy each pair (suppliers first).
pub fn compute_supply_unit_costs(model: &LpModel, costs: &[PairCost]) -> Vec<f64> {
    let n = model.n;
    let mut order = supplier_to_consumer_order(n, &model.bom_consumers, &model.transport_consumers);
    order.reverse();

    let edge_cost: HashMap<(usize, usize), f64> = model
        .transport_edges
        .iter()
        .map(|&(s, d, _, c)| ((s, d), c))
        .collect();
    let dynamic_dst: HashSet<usize> = model.dynamic_transport_edges.iter().map(|e| e.1).collect();
    let mut dynamic_inbound: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    for &(s, d, _lt, c) in &model.dynamic_transport_edges {
        dynamic_inbound.entry(d).or_default().push((s, c));
    }
    let mut static_inbound: HashMap<usize, Vec<(usize, f64, f64)>> = HashMap::new();
    for (&src_p, edges) in &model.transport_consumers {
        for &(dst_p, _lt, mult) in edges {
            if dynamic_dst.contains(&dst_p) {
                continue;
            }
            let rho = edge_cost.get(&(src_p, dst_p)).copied().unwrap_or(0.0);
            static_inbound.entry(dst_p).or_default().push((src_p, mult, rho));
        }
    }
    let mut parent_bom: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    for (&child_p, edges) in &model.bom_consumers {
        for (k, &(parent_p, factor, lt)) in edges.iter().enumerate() {
            if !model.bom_windows.active(child_p, k, 0, lt) {
                continue;
            }
            parent_bom.entry(parent_p).or_default().push((child_p, factor));
        }
    }

    let mut v = vec![0.0; n];
    for p in order {
        let pair = &model.all_pairs[p];
        let mut best: Option<f64> = None;
        let mut push = |c: f64| best = Some(best.map_or(c, |b: f64| b.min(c)));
        if pair.can_produce {
            let mat: f64 = parent_bom
                .get(&p)
                .map(|l| l.iter().map(|&(c, f)| f * v[c]).sum())
                .unwrap_or(0.0);
            push(costs[p].prod + mat);
        }
        if pair.can_replenish {
            if dynamic_dst.contains(&p) {
                if let Some(l) = dynamic_inbound.get(&p) {
                    push(l.iter().map(|&(s, c)| c + v[s]).fold(f64::INFINITY, f64::min));
                }
            } else if let Some(l) = static_inbound.get(&p) {
                push(l.iter().map(|&(s, m, c)| m * (c + v[s])).sum());
            }
        }
        if pair.can_procure {
            push(costs[p].procure);
        }
        v[p] = best.unwrap_or(0.0);
    }
    v
}

/// Build the backend-agnostic LP model (`solver_lp_model._build_lp_model`).
/// Returns `Ok(None)` when `facts` is empty or has no pairs.
pub fn build_lp_model(req: &OptimizeRequestPayload) -> Result<Option<LpModel>, OptimizerError> {
    if req.facts.is_empty() {
        return Ok(None);
    }

    let lookups = build_master_lookups(
        &req.lines,
        &req.bom,
        &req.transport_routes,
        &req.item_locations,
        &req.init_inventory,
        &req.locations,
        &req.line_eligibility,
        &req.uom_factors,
    );

    // Opening inventory defaults to the sum of the initial lots when no `init_inventory` row exists
    // (openSCM derives it from `inventory_lots`).
    let mut lookups = lookups;
    let mut lot_sum: HashMap<(String, String), f64> = HashMap::new();
    for l in &req.init_lots {
        if l.qty > 0.0 {
            *lot_sum.entry((l.location.clone(), l.sku.clone())).or_insert(0.0) += l.qty;
        }
    }
    for (k, q) in lot_sum {
        lookups.init_inv_map.entry(k).or_insert(q);
    }

    let mut ls_pairs_set: BTreeSet<(String, String)> = BTreeSet::new();
    let mut weeks_set: BTreeSet<String> = BTreeSet::new();
    for f in &req.facts {
        ls_pairs_set.insert((f.location.clone(), f.sku.clone()));
        weeks_set.insert(f.week_start.clone());
    }
    let weeks: Vec<String> = weeks_set.into_iter().collect();
    let t_len = weeks.len();
    assert_uniform_weekly_axis(&weeks)?;

    let manufactured_skus: HashSet<String> = lookups
        .bom_list
        .iter()
        .map(|r| r.parent_sku.clone())
        .collect();

    let facts_pairs: BTreeSet<(String, String)> = ls_pairs_set.clone();
    let mut all_pairs = Vec::with_capacity(ls_pairs_set.len());
    let mut pair_idx_map = HashMap::with_capacity(ls_pairs_set.len());
    for (loc, sku) in ls_pairs_set {
        let key = (loc.clone(), sku.clone());
        let can_produce =
            lookups.location_capacity.contains_key(&loc) && lookups.bom_keys.contains(&key);
        let can_replenish = lookups.transport_keys.contains(&key);
        let can_procure = match lookups.can_procure_override_map.get(&key) {
            Some(&cp) => cp,
            None => !can_produce && !can_replenish && !manufactured_skus.contains(&sku),
        };
        let p_idx = all_pairs.len();
        pair_idx_map.insert(key, p_idx);
        all_pairs.push(PairNode {
            location: loc,
            sku,
            can_produce,
            can_replenish,
            can_procure,
        });
    }

    // Ghost suppliers: pairs that feed a facts pair via transport/BOM but have no facts rows.
    let mut ghost_needed: BTreeSet<(String, String)> = BTreeSet::new();
    for r in &lookups.transport_list {
        if facts_pairs.contains(&(r.to_location.clone(), r.sku.clone())) {
            ghost_needed.insert((r.from_location.clone(), r.sku.clone()));
        }
    }
    for r in &lookups.bom_list {
        if facts_pairs.contains(&(r.target_location.clone(), r.parent_sku.clone())) {
            ghost_needed.insert((r.source_location.clone(), r.child_sku.clone()));
        }
    }
    let mut ghost_pairs: HashSet<usize> = HashSet::new();
    let mut warnings: Vec<ModelWarning> = Vec::new();
    for (loc, sku) in ghost_needed.difference(&facts_pairs) {
        let p_idx = all_pairs.len();
        ghost_pairs.insert(p_idx);
        pair_idx_map.insert((loc.clone(), sku.clone()), p_idx);
        all_pairs.push(PairNode {
            location: loc.clone(),
            sku: sku.clone(),
            can_produce: false,
            can_replenish: false,
            can_procure: false,
        });
        warnings.push(ModelWarning {
            code: "supplier_pair_missing_in_facts".to_string(),
            location: Some(loc.clone()),
            sku: Some(sku.clone()),
            week: None,
            message: "供給元の計画行が無いため、期首在庫と入荷予定だけを供給として扱いました。".to_string(),
        });
    }

    let n = all_pairs.len();
    if n == 0 {
        return Ok(None);
    }

    // BOM adjacency
    let mut bom_consumers: HashMap<usize, Vec<(usize, f64, i64)>> = HashMap::new();
    let mut bom_win_map: HashMap<usize, Vec<(Option<String>, Option<String>)>> = HashMap::new();
    for r in &lookups.bom_list {
        let src = (r.source_location.clone(), r.child_sku.clone());
        let parent = (r.target_location.clone(), r.parent_sku.clone());
        if let (Some(&src_p), Some(&parent_p)) = (pair_idx_map.get(&src), pair_idx_map.get(&parent))
        {
            let prod_lt = *lookups.prod_lt_map.get(&parent).unwrap_or(&0);
            let yr = *lookups.yield_map.get(&parent).unwrap_or(&1.0);
            let effective_qty = bom_factor(r.quantity, r.scrap_rate, yr);
            bom_consumers
                .entry(src_p)
                .or_default()
                .push((parent_p, effective_qty, prod_lt));
            bom_win_map
                .entry(src_p)
                .or_default()
                .push((r.valid_from.clone(), r.valid_to.clone()));
        }
    }

    // Quota normalisation + multi-source fallback
    let mut quota_groups: HashMap<(String, String), Vec<(usize, Option<f64>)>> = HashMap::new();
    for (r_idx, r) in lookups.transport_list.iter().enumerate() {
        let dst_key = (r.to_location.clone(), r.sku.clone());
        quota_groups
            .entry(dst_key)
            .or_default()
            .push((r_idx, r.quota_pct));
    }

    let mut route_multiplier: Vec<f64> = vec![1.0; lookups.transport_list.len()];
    let mut has_quota_split = false;
    let mut dynamic_dsts: HashSet<(String, String)> = HashSet::new();
    for (dst_key, entries) in &quota_groups {
        if entries.len() >= 2 && entries.iter().all(|(_, p)| p.is_some()) {
            let total: f64 = entries.iter().filter_map(|(_, p)| *p).sum();
            let denom = if total > 0.0 { total } else { 1.0 };
            for &(r_idx, p_opt) in entries {
                route_multiplier[r_idx] = p_opt.unwrap_or(0.0) / denom;
            }
            has_quota_split = true;
        } else {
            if req.config.dynamic_sourcing && entries.len() >= 2 {
                dynamic_dsts.insert(dst_key.clone());
            }
            // Winner by (transport_lt, transport_cost_per_unit) ascending; tie-broken by first index
            let winner_idx = entries
                .iter()
                .min_by(|&&(idx_a, _), &&(idx_b, _)| {
                    let ra = &lookups.transport_list[idx_a];
                    let rb = &lookups.transport_list[idx_b];
                    let lt_a = ra.transport_lt.unwrap_or(0);
                    let lt_b = rb.transport_lt.unwrap_or(0);
                    let cost_a = ra.transport_cost_per_unit.unwrap_or(0.0);
                    let cost_b = rb.transport_cost_per_unit.unwrap_or(0.0);
                    lt_a.cmp(&lt_b)
                        .then_with(|| cost_a.partial_cmp(&cost_b).unwrap_or(std::cmp::Ordering::Equal))
                        .then_with(|| idx_a.cmp(&idx_b))
                })
                .map(|&(idx, _)| idx)
                .unwrap();
            for &(r_idx, _) in entries {
                route_multiplier[r_idx] = if r_idx == winner_idx { 1.0 } else { 0.0 };
            }
        }
    }

    let mut transport_consumers: HashMap<usize, Vec<(usize, i64, f64)>> = HashMap::new();
    let mut transport_edges: Vec<(usize, usize, i64, f64)> = Vec::new();
    let mut replen_cost_by_dst: HashMap<usize, f64> = HashMap::new();
    let mut dynamic_transport_edges: Vec<(usize, usize, i64, f64)> = Vec::new();
    for (r_idx, r) in lookups.transport_list.iter().enumerate() {
        let src = (r.from_location.clone(), r.sku.clone());
        let dst = (r.to_location.clone(), r.sku.clone());
        if let (Some(&src_p), Some(&dst_p)) = (pair_idx_map.get(&src), pair_idx_map.get(&dst)) {
            let trans_lt = r.transport_lt.unwrap_or(0);
            let cost = route_unit_cost(r.transport_cost_per_unit, &req.config);
            let mult = route_multiplier[r_idx];
            if mult > 0.0 {
                *replen_cost_by_dst.entry(dst_p).or_insert(0.0) += mult * cost;
            }
            let is_dynamic = dynamic_dsts.contains(&dst);
            if is_dynamic {
                transport_edges.push((src_p, dst_p, trans_lt, cost));
                dynamic_transport_edges.push((src_p, dst_p, trans_lt, cost));
            }
            if mult <= 0.0 {
                continue;
            }
            if !is_dynamic {
                transport_edges.push((src_p, dst_p, trans_lt, cost));
            }
            transport_consumers
                .entry(src_p)
                .or_default()
                .push((dst_p, trans_lt, mult));
        }
    }

    // PMU-15 supply lead times
    let mut supply_p_lt: HashMap<usize, i64> = HashMap::new();
    for (p, pair) in all_pairs.iter().enumerate() {
        if pair.can_produce {
            let key = (pair.location.clone(), pair.sku.clone());
            let lt = *lookups
                .prod_block_lt_map
                .get(&key)
                .or_else(|| lookups.prod_lt_map.get(&key))
                .unwrap_or(&0);
            supply_p_lt.insert(p, lt);
        }
    }
    let mut supply_r_lt: HashMap<usize, i64> = HashMap::new();
    for consumers in transport_consumers.values() {
        for &(dst_p, trans_lt, _) in consumers {
            let entry = supply_r_lt.entry(dst_p).or_insert(0);
            if trans_lt > *entry {
                *entry = trans_lt;
            }
        }
    }

    // Demand and facts_lookup
    let mut demand = vec![vec![0.0; t_len]; n];
    let mut demand_for_target = vec![vec![0.0; t_len]; n];
    let mut facts_lookup: HashMap<(String, String, String), FactRow> =
        HashMap::with_capacity(req.facts.len());
    for f in &req.facts {
        facts_lookup.insert(
            (f.location.clone(), f.sku.clone(), f.week_start.clone()),
            f.clone(),
        );
    }
    for (p, pair) in all_pairs.iter().enumerate() {
        for (t, w) in weeks.iter().enumerate() {
            if let Some(row) =
                facts_lookup.get(&(pair.location.clone(), pair.sku.clone(), w.clone()))
            {
                demand[p][t] = row.sales_qty;
                demand_for_target[p][t] = row.sales_qty;
            }
        }
    }

    let bom_windows = BomWindows { windows: bom_win_map, weeks: weeks.clone() };
    propagate_downstream_demand(
        &mut demand_for_target,
        &bom_consumers,
        &transport_consumers,
        n,
        t_len,
        &bom_windows,
    );

    let mut target_inv = vec![vec![0.0; t_len]; n];
    for (p, pair) in all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        // Check item_locations ss_weeks_map first, or fallback to FactRow.safety_stock_weeks
        let ss_weeks = lookups
            .ss_weeks_map
            .get(&key)
            .copied()
            .or_else(|| {
                weeks.first().and_then(|w0| {
                    facts_lookup
                        .get(&(pair.location.clone(), pair.sku.clone(), w0.clone()))
                        .and_then(|f| f.safety_stock_weeks)
                })
            })
            .unwrap_or(0.0);
        if ss_weeks > 0.0 {
            target_inv[p] = compute_safety_stock_target(
                &demand_for_target[p],
                ss_weeks,
                req.config.terminal_lookback_weeks,
            );
        }
    }

    // Scheduled receipts
    let mut receipts_opt = None;
    if !req.scheduled_receipts.is_empty() {
        let receipts_map = bucket_receipts_by_week(&req.scheduled_receipts, &weeks);
        if !receipts_map.is_empty() {
            let mut r_mat = vec![vec![0.0; t_len]; n];
            let week_idx: HashMap<&str, usize> = weeks
                .iter()
                .enumerate()
                .map(|(t, w)| (w.as_str(), t))
                .collect();
            for ((loc, sku), weekmap) in receipts_map {
                if let Some(&p) = pair_idx_map.get(&(loc, sku)) {
                    for (w, qty) in weekmap {
                        if let Some(&t) = week_idx.get(w.as_str()) {
                            r_mat[p][t] += qty;
                        }
                    }
                }
            }
            receipts_opt = Some(r_mat);
        }
    }

    let calendar = build_capacity_calendar(
        &lookups.line_capacity,
        &lookups.location_capacity,
        &lookups.line_hours,
        &req.capacity_overrides,
        &weeks,
    );

    let value_added_map = compute_value_added_costs(&lookups);

    // Projected FEFO expiries of initial lots.
    let mut projected_expiries: Option<Vec<Vec<f64>>> = None;
    if !req.init_lots.is_empty() {
        let week_dates: Vec<NaiveDate> = weeks
            .iter()
            .filter_map(|w| NaiveDate::parse_from_str(w, "%Y-%m-%d").ok())
            .collect();
        if week_dates.len() == weeks.len() {
            let mut by_pair: HashMap<(String, String), Vec<(f64, NaiveDate)>> = HashMap::new();
            for l in &req.init_lots {
                if l.qty <= 0.0 {
                    continue;
                }
                let Some(exp) = l.expiry_week.as_deref() else { continue };
                let exp10: String = exp.chars().take(10).collect();
                let Ok(d) = NaiveDate::parse_from_str(&exp10, "%Y-%m-%d") else { continue };
                by_pair
                    .entry((l.location.clone(), l.sku.clone()))
                    .or_default()
                    .push((l.qty, d));
            }
            for (key, lots) in by_pair {
                let Some(&p) = pair_idx_map.get(&key) else { continue };
                let min_rem = lookups.min_rem_shelf_map.get(&key).copied().unwrap_or(0);
                let exp = compute_projected_lot_expiries(&lots, &demand_for_target[p], &week_dates, min_rem);
                if !exp.is_empty() {
                    let m = projected_expiries.get_or_insert_with(|| vec![vec![0.0; t_len]; n]);
                    for (t, q) in exp {
                        m[p][t] += q;
                    }
                }
            }
        }
    }

    // Firm purchases, protected cells.
    let mut firm_purchase: HashMap<(usize, usize), f64> = HashMap::new();
    for (p, pair) in all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let e_lt = lookups.procurement_lt_map.get(&key).copied().unwrap_or(0);
        for t in 0..t_len {
            let row = facts_lookup.get(&(key.0.clone(), key.1.clone(), weeks[t].clone()));
            if !cell_protected(row, t, true) {
                continue;
            }
            let r_qty = row.map(|r| r.replenishment_qty as f64).unwrap_or(0.0);
            let a = if !pair.can_procure {
                0.0
            } else if !pair.can_replenish {
                r_qty
            } else {
                let te = t as i64 - e_lt;
                if te >= 0 {
                    let rel = facts_lookup
                        .get(&(key.0.clone(), key.1.clone(), weeks[te as usize].clone()))
                        .and_then(|r| r.order_release_qty)
                        .unwrap_or(0) as f64;
                    r_qty.min(rel)
                } else {
                    0.0
                }
            };
            firm_purchase.insert((p, t), a);
        }
    }
    let mut firm_purchase_rhs: HashMap<(usize, usize), f64> = HashMap::new();
    let mut protected_p: HashSet<(usize, usize)> = HashSet::new();
    let mut protected_r: HashSet<(usize, usize)> = HashSet::new();
    let mut protected_e: HashSet<(usize, usize)> = HashSet::new();
    for (&(p, t), &a) in &firm_purchase {
        let key = (all_pairs[p].location.clone(), all_pairs[p].sku.clone());
        let e_lt = lookups.procurement_lt_map.get(&key).copied().unwrap_or(0);
        let te = t as i64 - e_lt;
        if te < 0 {
            firm_purchase_rhs.insert((p, t), a);
        } else {
            protected_e.insert((p, te as usize));
        }
    }
    for (p, pair) in all_pairs.iter().enumerate() {
        for t in 0..t_len {
            let row = facts_lookup.get(&(pair.location.clone(), pair.sku.clone(), weeks[t].clone()));
            if pair.can_produce && cell_protected(row, t, false) {
                protected_p.insert((p, t));
            }
            if pair.can_replenish && cell_protected(row, t, true) {
                protected_r.insert((p, t));
            }
        }
    }
    let has_firm_cells = !(protected_p.is_empty() && protected_r.is_empty() && protected_e.is_empty());

    // Alternate lines.
    let mut alt_line_edges: Vec<AltLineEdge> = Vec::new();
    for (p, pair) in all_pairs.iter().enumerate() {
        if !pair.can_produce {
            continue;
        }
        let key = (pair.location.clone(), pair.sku.clone());
        let Some(elig) = lookups.line_eligibility_map.get(&key) else { continue };
        let prim_line = lookups.sku_to_line.get(&key);
        let base_cost = lookups.cost_map.get(&key).copied().unwrap_or(req.config.production_cost);
        for (rank_i, (alt_line, eff)) in elig.iter().enumerate() {
            if Some(alt_line) == prim_line {
                continue;
            }
            let eff_val = eff.max(0.01);
            alt_line_edges.push(AltLineEdge {
                p,
                location: pair.location.clone(),
                sku: pair.sku.clone(),
                alt_line: alt_line.clone(),
                efficiency: eff_val,
                alt_cost: base_cost / eff_val + 0.01 * (rank_i as f64 + 1.0),
            });
        }
    }

    let backorder_sku_set: HashSet<&str> = req.backorder_skus.iter().map(|s| s.as_str()).collect();
    let backorder_pairs: HashSet<usize> = all_pairs
        .iter()
        .enumerate()
        .filter(|(_, pair)| backorder_sku_set.contains(pair.sku.as_str()))
        .map(|(p, _)| p)
        .collect();

    Ok(Some(LpModel {
        lookups,
        all_pairs,
        pair_idx_map,
        n,
        t_len,
        weeks,
        facts_lookup,
        demand,
        target_inv,
        receipts: receipts_opt,
        calendar,
        backorder_pairs,
        bom_consumers,
        bom_windows,
        transport_consumers,
        transport_edges,
        has_quota_split,
        supply_p_lt,
        supply_r_lt,
        replen_cost_by_dst,
        value_added_map,
        ghost_pairs,
        projected_expiries,
        alt_line_edges,
        dynamic_transport_edges,
        firm_purchase,
        firm_purchase_rhs,
        protected_p,
        protected_r,
        protected_e,
        has_firm_cells,
        relax_firm: false,
        warnings,
    }))
}

/// Sparse linear constraint row: `sum(coeff * x[var_idx]) == rhs` or `<= rhs`.
#[derive(Debug, Clone)]
pub struct SparseRow {
    pub terms: Vec<(usize, f64)>,
    pub rhs: f64,
    /// Optional human-readable description if this inequality constraint is a capacity/storage bottleneck candidate.
    pub bottleneck_desc: Option<String>,
}

/// Complete assembled matrix problem ready for an LP solver.
#[derive(Debug, Clone)]
pub struct FormulatedLp {
    pub num_vars: usize,
    pub c: Vec<f64>,
    /// Variable bounds `(lb, ub)` where `ub = f64::INFINITY` means unbounded above.
    pub bounds: Vec<(f64, f64)>,
    pub eq_rows: Vec<SparseRow>,
    pub ub_rows: Vec<SparseRow>,
    /// First column of the alternate-line `PA` block (`alt_line_edges.len() * t_len` columns).
    pub pa_base: Option<usize>,
    /// `(location, line, week) -> index into ub_rows` of the line capacity row (used by the MILP setup-time term).
    pub line_row_idx: HashMap<(String, String, usize), usize>,
    /// `(location, line) -> (time_based, line_hours, max_qty_capacity)`.
    pub line_mode: HashMap<(String, String), (bool, f64, f64)>,
}

/// openSCM `_highs_apply_relax_firm`: patch an already formulated (strict) LP in place so that
/// protected P/R/E columns get lower bound 0 (upper bound kept) and cost `-firm_reward`.
/// Everything else (incl. Network-Design `TR` pinning) stays exactly as built.
pub fn apply_relax_firm(model: &LpModel, cfg: &SolverConfig, lp: &mut FormulatedLp) {
    let t_len = model.t_len;
    let mut max_shortage = cfg.shortage_penalty;
    for pr in model.lookups.price_map.values() {
        max_shortage = max_shortage.max(*pr);
    }
    let firm_reward = 10.0 * max_shortage;
    let mut patch = |vi: usize| {
        lp.bounds[vi].0 = 0.0;
        lp.c[vi] = -firm_reward;
    };
    for &(p, t) in &model.protected_p {
        patch(var_idx(p, t, SLOT_P, t_len));
    }
    for &(p, t) in &model.protected_r {
        patch(var_idx(p, t, SLOT_R, t_len));
    }
    for &(p, t) in &model.protected_e {
        patch(var_idx(p, t, SLOT_E, t_len));
    }
}

/// Formulate the full LP (variables, bounds, objective, equality and inequality constraints,
/// Network Design `TR` edges, and plan-stability `D` variables).
pub fn formulate_lp(model: &LpModel, cfg: &SolverConfig) -> FormulatedLp {
    let n = model.n;
    let t_len = model.t_len;
    let base_vars = n * t_len * V_SLOTS;
    let mut bounds = vec![(0.0, f64::INFINITY); base_vars];

    // Firm purchase pins E at the order week `t - e_lt` (arrival week is protected).
    let mut firm_e_at: HashMap<(usize, usize), f64> = HashMap::new();
    for (&(p, t), &a) in &model.firm_purchase {
        let key = (model.all_pairs[p].location.clone(), model.all_pairs[p].sku.clone());
        let e_lt = model.lookups.procurement_lt_map.get(&key).copied().unwrap_or(0);
        let te = t as i64 - e_lt;
        if te >= 0 {
            firm_e_at.insert((p, te as usize), a);
        }
    }

    // 1. Variable bounds (`_build_lp_variables`)
    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let pcap = model.lookups.procurement_capacity_map.get(&key).copied();
        let e_lt = if pair.can_procure {
            *model.lookups.procurement_lt_map.get(&key).unwrap_or(&0)
        } else {
            0
        };
        let p_lt_val = *model.supply_p_lt.get(&p).unwrap_or(&0);
        let r_lt_val = *model.supply_r_lt.get(&p).unwrap_or(&0);

        for t in 0..t_len {
            let t_i64 = t as i64;
            let row = model
                .facts_lookup
                .get(&(pair.location.clone(), pair.sku.clone(), model.weeks[t].clone()));
            let p_fence = row.and_then(|r| r.production_fence_weeks).unwrap_or(0);
            let r_fence = row.and_then(|r| r.replenishment_fence_weeks).unwrap_or(0);

            let _ = (p_fence, r_fence);
            if !pair.can_produce {
                bounds[var_idx(p, t, SLOT_P, t_len)] = (0.0, 0.0);
            } else if cell_protected(row, t, false) {
                let p_qty = row.map(|r| r.production_qty as f64).unwrap_or(0.0);
                bounds[var_idx(p, t, SLOT_P, t_len)] = (p_qty, p_qty);
            } else if t_i64 < p_lt_val {
                bounds[var_idx(p, t, SLOT_P, t_len)] = (0.0, 0.0);
            }

            if !pair.can_replenish {
                bounds[var_idx(p, t, SLOT_R, t_len)] = (0.0, 0.0);
            } else if cell_protected(row, t, true) {
                let fp = model.firm_purchase.get(&(p, t)).copied().unwrap_or(0.0);
                let r_qty = (row.map(|r| r.replenishment_qty as f64).unwrap_or(0.0) - fp).max(0.0);
                bounds[var_idx(p, t, SLOT_R, t_len)] = (r_qty, r_qty);
            } else if t_i64 < r_lt_val {
                bounds[var_idx(p, t, SLOT_R, t_len)] = (0.0, 0.0);
            }

            if !pair.can_procure {
                bounds[var_idx(p, t, SLOT_E, t_len)] = (0.0, 0.0);
            } else if e_lt > 0 && (t_i64 + e_lt) > (t_len as i64 - 1) {
                bounds[var_idx(p, t, SLOT_E, t_len)] = (0.0, 0.0);
            } else if let Some(cap) = pcap {
                bounds[var_idx(p, t, SLOT_E, t_len)] = (0.0, cap);
            }

            if let Some(&a) = firm_e_at.get(&(p, t)) {
                bounds[var_idx(p, t, SLOT_E, t_len)] = (a, a);
            }

            let u_ub = if model.backorder_pairs.contains(&p) {
                model.demand[p][0..=t].iter().sum()
            } else {
                model.demand[p][t]
            };
            bounds[var_idx(p, t, SLOT_U, t_len)] = (0.0, u_ub);
        }
    }

    // 2. Objective (`_build_objective`)
    let mut c = vec![0.0; base_vars];
    let max_prio = model.lookups.allocation_priority_map.values().copied().max().unwrap_or(0);
    let pair_costs: Vec<PairCost> = (0..n)
        .map(|p| compute_pair_cost(model, p, cfg, max_prio))
        .collect();
    let supply_unit_cost = compute_supply_unit_costs(model, &pair_costs);
    let kappa = cfg.terminal_inventory_value_ratio.clamp(0.0, 0.99);
    for (p, pair) in model.all_pairs.iter().enumerate() {
        let pc = pair_costs[p];
        let v = supply_unit_cost[p];
        let mut ss_pen = cfg.ss_violation_penalty;
        if v > 0.0 {
            ss_pen = ss_pen.max((cfg.ss_penalty_cost_ratio * v).min(0.5 * pc.shortage));
        }

        for t in 0..t_len {
            c[var_idx(p, t, SLOT_I, t_len)] = pc.hc;
            if model.backorder_pairs.contains(&p) {
                c[var_idx(p, t, SLOT_U, t_len)] = cfg.backlog_penalty_per_week
                    + if t == t_len - 1 { pc.shortage } else { 0.0 };
            } else {
                c[var_idx(p, t, SLOT_U, t_len)] = pc.shortage;
            }
            c[var_idx(p, t, SLOT_E, t_len)] = pc.procure;
            c[var_idx(p, t, SLOT_SS, t_len)] = ss_pen;
            if pair.can_produce {
                c[var_idx(p, t, SLOT_P, t_len)] = pc.prod;
            }
            if pair.can_replenish {
                c[var_idx(p, t, SLOT_R, t_len)] = pc.replen;
            }
        }
        if v > 0.0 {
            // Terminal inventory valuation: ending stock is worth kappa * v.
            c[var_idx(p, t_len - 1, SLOT_I, t_len)] = pc.hc - kappa * v;
        }
    }

    // Determine whether Network Design transport edges are used
    let use_network_design =
        cfg.formulation_mode != "glop" && !model.has_quota_split && !model.transport_edges.is_empty();

    // 3. Equality constraints: inventory balance (`_build_constraints`)
    let mut eq_rows: Vec<SparseRow> = Vec::with_capacity(n * t_len);
    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let init_i = *model.lookups.init_inv_map.get(&key).unwrap_or(&0.0);
        let e_lt = if pair.can_procure {
            *model.lookups.procurement_lt_map.get(&key).unwrap_or(&0)
        } else {
            0
        };
        for t in 0..t_len {
            let mut coeff_map: HashMap<usize, f64> = HashMap::new();
            let mut rhs = model.demand[p][t] - if t == 0 { init_i } else { 0.0 };
            if let Some(ref rec) = model.receipts {
                rhs -= rec[p][t];
            }
            if let Some(&a) = model.firm_purchase_rhs.get(&(p, t)) {
                rhs -= a;
            }
            if let Some(ref ex) = model.projected_expiries {
                rhs += ex[p][t];
            }

            if t > 0 {
                coeff_map.insert(var_idx(p, t - 1, SLOT_I, t_len), 1.0);
            }
            if pair.can_produce {
                coeff_map.insert(var_idx(p, t, SLOT_P, t_len), 1.0);
            }
            if pair.can_replenish {
                coeff_map.insert(var_idx(p, t, SLOT_R, t_len), 1.0);
            }
            if pair.can_procure && (t as i64) - e_lt >= 0 {
                let t_order = ((t as i64) - e_lt) as usize;
                coeff_map.insert(var_idx(p, t_order, SLOT_E, t_len), 1.0);
            }
            coeff_map.insert(var_idx(p, t, SLOT_U, t_len), 1.0);
            if model.backorder_pairs.contains(&p) && t > 0 {
                coeff_map.insert(var_idx(p, t - 1, SLOT_U, t_len), -1.0);
            }
            coeff_map.insert(var_idx(p, t, SLOT_I, t_len), -1.0);

            if let Some(bom_edges) = model.bom_consumers.get(&p) {
                for (k, &(parent_p, bom_qty, prod_lt)) in bom_edges.iter().enumerate() {
                    let lt_u = prod_lt.max(0) as usize;
                    if t == 0 {
                        let max_tp = lt_u.min(t_len - 1);
                        for t_parent in 0..=max_tp {
                            if !model.bom_windows.active(p, k, t_parent, prod_lt) {
                                continue;
                            }
                            *coeff_map
                                .entry(var_idx(parent_p, t_parent, SLOT_P, t_len))
                                .or_insert(0.0) -= bom_qty;
                        }
                    } else {
                        let t_parent = t + lt_u;
                        if t_parent < t_len && model.bom_windows.active(p, k, t_parent, prod_lt) {
                            *coeff_map
                                .entry(var_idx(parent_p, t_parent, SLOT_P, t_len))
                                .or_insert(0.0) -= bom_qty;
                        }
                    }
                }
            }

            if !use_network_design {
                if let Some(tr_edges) = model.transport_consumers.get(&p) {
                    for &(dst_p, trans_lt, mult) in tr_edges {
                        let lt_u = trans_lt.max(0) as usize;
                        if t == 0 {
                            let max_td = lt_u.min(t_len - 1);
                            for t_downstream in 0..=max_td {
                                *coeff_map
                                    .entry(var_idx(dst_p, t_downstream, SLOT_R, t_len))
                                    .or_insert(0.0) -= mult;
                            }
                        } else {
                            let t_downstream = t + lt_u;
                            if t_downstream < t_len {
                                *coeff_map
                                    .entry(var_idx(dst_p, t_downstream, SLOT_R, t_len))
                                    .or_insert(0.0) -= mult;
                            }
                        }
                    }
                }
            }

            let mut terms: Vec<(usize, f64)> = coeff_map
                .into_iter()
                .filter(|&(_, v)| v.abs() > 1e-15)
                .collect();
            terms.sort_by_key(|&(col, _)| col);
            eq_rows.push(SparseRow {
                terms,
                rhs,
                bottleneck_desc: None,
            });
        }
    }

    // 4. Inequality constraints: Line capacity, Location capacity, Target floor, Storage capacity
    let mut ub_rows: Vec<SparseRow> = Vec::new();

    // Preserve deterministic iteration order matching `all_pairs` insertion order
    let mut line_order: Vec<(String, String)> = Vec::new();
    let mut line_to_pairs: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut unassigned_order: Vec<String> = Vec::new();
    let mut unassigned_pairs: HashMap<String, Vec<usize>> = HashMap::new();

    for (p, pair) in model.all_pairs.iter().enumerate() {
        if pair.can_produce {
            let key = (pair.location.clone(), pair.sku.clone());
            if let Some(line) = model.lookups.sku_to_line.get(&key) {
                let lk = (pair.location.clone(), line.clone());
                if !line_to_pairs.contains_key(&lk) {
                    line_order.push(lk.clone());
                }
                line_to_pairs.entry(lk).or_default().push(p);
            } else {
                if !unassigned_pairs.contains_key(&pair.location) {
                    unassigned_order.push(pair.location.clone());
                }
                unassigned_pairs
                    .entry(pair.location.clone())
                    .or_default()
                    .push(p);
            }
        }
    }

    let mut line_row_idx: HashMap<(String, String, usize), usize> = HashMap::new();
    let mut line_time_based: HashMap<(String, String), bool> = HashMap::new();
    for (loc, line) in line_order {
        let pairs = &line_to_pairs[&(loc.clone(), line.clone())];
        let lk = (loc.clone(), line.clone());
        let hours = *model.lookups.line_hours.get(&lk).unwrap_or(&0.0);
        let max_cap = *model.lookups.line_capacity.get(&lk).unwrap_or(&0.0);
        let use_time_based = hours > 0.0
            && pairs.iter().all(|&p| {
                let k = (
                    model.all_pairs[p].location.clone(),
                    model.all_pairs[p].sku.clone(),
                );
                model.lookups.sku_production_rate.contains_key(&k)
            });

        line_time_based.insert((loc.clone(), line.clone()), use_time_based);
        for t in 0..t_len {
            let w = &model.weeks[t];
            line_row_idx.insert((loc.clone(), line.clone(), t), ub_rows.len());
            if use_time_based {
                let terms: Vec<(usize, f64)> = pairs
                    .iter()
                    .map(|&p| {
                        let k = (
                            model.all_pairs[p].location.clone(),
                            model.all_pairs[p].sku.clone(),
                        );
                        let rate_info = &model.lookups.sku_production_rate[&k];
                        let coeff = 1.0 / (rate_info.rate_per_hour * rate_info.yield_rate);
                        (var_idx(p, t, SLOT_P, t_len), coeff)
                    })
                    .collect();
                let rhs = *model
                    .calendar
                    .hours_t
                    .get(&(loc.clone(), line.clone(), t))
                    .unwrap_or(&hours);
                ub_rows.push(SparseRow {
                    terms,
                    rhs,
                    bottleneck_desc: Some(format!(
                        "Line Time Capacity at {loc} ({line}) in Week {w}"
                    )),
                });
            } else {
                let terms: Vec<(usize, f64)> = pairs
                    .iter()
                    .map(|&p| (var_idx(p, t, SLOT_P, t_len), 1.0))
                    .collect();
                let rhs = *model
                    .calendar
                    .line_cap_t
                    .get(&(loc.clone(), line.clone(), t))
                    .unwrap_or(&max_cap);
                ub_rows.push(SparseRow {
                    terms,
                    rhs,
                    bottleneck_desc: Some(format!(
                        "Line Qty Capacity at {loc} ({line}) in Week {w}"
                    )),
                });
            }
        }
    }

    // openSCM `location_capacity_groups`: the location row covers every producible pair of a
    // location that has at least one pair without a line.
    let mut loc_producible: HashMap<String, Vec<usize>> = HashMap::new();
    for (p, pair) in model.all_pairs.iter().enumerate() {
        if pair.can_produce {
            loc_producible.entry(pair.location.clone()).or_default().push(p);
        }
    }
    for loc in unassigned_order {
        let pairs = &loc_producible[&loc];
        let max_cap = *model.lookups.location_capacity.get(&loc).unwrap_or(&0.0);
        for t in 0..t_len {
            let w = &model.weeks[t];
            let terms: Vec<(usize, f64)> = pairs
                .iter()
                .map(|&p| (var_idx(p, t, SLOT_P, t_len), 1.0))
                .collect();
            let rhs = *model
                .calendar
                .loc_cap_t
                .get(&(loc.clone(), t))
                .unwrap_or(&max_cap);
            ub_rows.push(SparseRow {
                terms,
                rhs,
                bottleneck_desc: Some(format!("Location Capacity at {loc} in Week {w}")),
            });
        }
    }

    // Safety-stock floor: -I[t] - SS[t] <= -target_inv[t]
    for p in 0..n {
        for t in 0..t_len {
            let target = model.target_inv[p][t];
            if target > 0.0 {
                ub_rows.push(SparseRow {
                    terms: vec![
                        (var_idx(p, t, SLOT_I, t_len), -1.0),
                        (var_idx(p, t, SLOT_SS, t_len), -1.0),
                    ],
                    rhs: -target,
                    bottleneck_desc: None,
                });
            }
        }
    }

    // Storage capacity
    if !model.lookups.storage_capacity_map.is_empty() {
        let mut loc_order: Vec<String> = Vec::new();
        let mut loc_to_pairs: HashMap<String, Vec<usize>> = HashMap::new();
        for (p, pair) in model.all_pairs.iter().enumerate() {
            if !loc_to_pairs.contains_key(&pair.location) {
                loc_order.push(pair.location.clone());
            }
            loc_to_pairs.entry(pair.location.clone()).or_default().push(p);
        }
        for loc in loc_order {
            if let Some(&max_cap) = model.lookups.storage_capacity_map.get(&loc) {
                let pairs = &loc_to_pairs[&loc];
                for t in 0..t_len {
                    let w = &model.weeks[t];
                    let suom = model.lookups.storage_uom_map.get(&loc);
                    let terms: Vec<(usize, f64)> = pairs
                        .iter()
                        .map(|&p| {
                            let coeff = crate::lookups::storage_uom_coeff(
                                &model.all_pairs[p].sku,
                                suom,
                                &model.lookups.uom_factors,
                            );
                            (var_idx(p, t, SLOT_I, t_len), coeff)
                        })
                        .collect();
                    ub_rows.push(SparseRow {
                        terms,
                        rhs: max_cap,
                        bottleneck_desc: Some(format!("Storage Capacity at {loc} in Week {w}")),
                    });
                }
            }
        }
    }

    let mut num_vars = base_vars;

    // 4b. Alternate-line spillover variables `PA_{e,t}` (`_add_alt_line_vars`).
    let mut pa_base: Option<usize> = None;
    if !model.alt_line_edges.is_empty() {
        let base = num_vars;
        pa_base = Some(base);
        let n_pa = model.alt_line_edges.len() * t_len;
        bounds.resize(base + n_pa, (0.0, f64::INFINITY));
        c.resize(base + n_pa, 0.0);

        for (e_idx, e) in model.alt_line_edges.iter().enumerate() {
            let key = (e.location.clone(), e.sku.clone());
            let base_cost = model.lookups.cost_map.get(&key).copied().unwrap_or(cfg.production_cost);
            let extra = (e.alt_cost - base_cost).max(0.01);
            let p_lt = *model.supply_p_lt.get(&e.p).unwrap_or(&0);
            for t in 0..t_len {
                let row = model.facts_lookup.get(&(e.location.clone(), e.sku.clone(), model.weeks[t].clone()));
                if cell_protected(row, t, false) || (t as i64) < p_lt {
                    bounds[base + e_idx * t_len + t] = (0.0, 0.0);
                }
                c[base + e_idx * t_len + t] = extra;
            }
        }

        // Alternate lines without primary SKUs still need a capacity row.
        let mut alt_lines_needed: BTreeSet<(String, String)> = BTreeSet::new();
        for e in &model.alt_line_edges {
            alt_lines_needed.insert((e.location.clone(), e.alt_line.clone()));
        }
        for (loc_a, ln_a) in &alt_lines_needed {
            if line_row_idx.contains_key(&(loc_a.clone(), ln_a.clone(), 0)) {
                continue;
            }
            let max_c = model.lookups.line_capacity.get(&(loc_a.clone(), ln_a.clone())).copied().unwrap_or(0.0);
            for t in 0..t_len {
                line_row_idx.insert((loc_a.clone(), ln_a.clone(), t), ub_rows.len());
                let rhs = *model.calendar.line_cap_t.get(&(loc_a.clone(), ln_a.clone(), t)).unwrap_or(&max_c);
                ub_rows.push(SparseRow {
                    terms: Vec::new(),
                    rhs,
                    bottleneck_desc: Some(format!(
                        "Line Qty Capacity at {loc_a} ({ln_a}) in Week {}",
                        model.weeks[t]
                    )),
                });
            }
        }

        // PA cannot exceed the SKU's own production: -P + sum(PA) <= 0
        let mut p_to_edges: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (e_idx, e) in model.alt_line_edges.iter().enumerate() {
            p_to_edges.entry(e.p).or_default().push(e_idx);
        }
        for (p_idx, edges) in &p_to_edges {
            for t in 0..t_len {
                let mut terms = vec![(var_idx(*p_idx, t, SLOT_P, t_len), -1.0)];
                for &e_idx in edges {
                    terms.push((base + e_idx * t_len + t, 1.0));
                }
                ub_rows.push(SparseRow { terms, rhs: 0.0, bottleneck_desc: None });
            }
        }

        // Capacity consumption on primary and alternate lines.
        for (e_idx, e) in model.alt_line_edges.iter().enumerate() {
            let key = (e.location.clone(), e.sku.clone());
            let prim_line = model.lookups.sku_to_line.get(&key);
            let prim_time = prim_line
                .and_then(|l| line_time_based.get(&(e.location.clone(), l.clone())))
                .copied()
                .unwrap_or(false);
            let alt_time = line_time_based
                .get(&(e.location.clone(), e.alt_line.clone()))
                .copied()
                .unwrap_or(false);
            let rate = model.lookups.sku_production_rate.get(&key);
            for t in 0..t_len {
                let pa_v = base + e_idx * t_len + t;
                if let Some(pl) = prim_line {
                    if let Some(&r) = line_row_idx.get(&(e.location.clone(), pl.clone(), t)) {
                        let coeff = match (prim_time, rate) {
                            (true, Some(ri)) => 1.0 / (ri.rate_per_hour * ri.yield_rate),
                            _ => 1.0,
                        };
                        ub_rows[r].terms.push((pa_v, -coeff));
                    }
                }
                if let Some(&r) = line_row_idx.get(&(e.location.clone(), e.alt_line.clone(), t)) {
                    let coeff = match (alt_time, rate) {
                        (true, Some(ri)) => 1.0 / (ri.rate_per_hour * ri.yield_rate * e.efficiency),
                        _ => 1.0 / e.efficiency,
                    };
                    ub_rows[r].terms.push((pa_v, coeff));
                }
            }
        }
        num_vars += n_pa;
    }

    // 5. Network Design transport edge variables (`TR`) when `use_network_design` is true
    if use_network_design {
        let tr_base = num_vars;
        let n_tr = model.transport_edges.len() * t_len;
        bounds.resize(tr_base + n_tr, (0.0, f64::INFINITY));
        c.resize(tr_base + n_tr, 0.0);

        for (e_idx, &(src_p, dst_p, trans_lt, cost)) in model.transport_edges.iter().enumerate() {
            let lt_u = trans_lt.max(0) as usize;
            for t in 0..lt_u.min(t_len) {
                if bounds[var_idx(dst_p, t, SLOT_R, t_len)].0 <= 0.0 {
                    bounds[tr_base + e_idx * t_len + t] = (0.0, 0.0);
                }
            }
            for t in 0..t_len {
                let tr_var_idx = tr_base + e_idx * t_len + t;
                c[tr_var_idx] = cost;
                // Outbound consumption at source
                let row_idx = if t >= lt_u {
                    src_p * t_len + (t - lt_u)
                } else {
                    src_p * t_len
                };
                eq_rows[row_idx].terms.push((tr_var_idx, -1.0));
            }
        }

        // Move R costs to TR
        for p in 0..n {
            for t in 0..t_len {
                c[var_idx(p, t, SLOT_R, t_len)] = 0.0;
            }
        }

        // R[dst_p, t] - sum(TR[src_p, dst_p, t]) == 0
        let mut dst_set: BTreeSet<usize> = BTreeSet::new();
        for &(_, dst_p, _, _) in &model.transport_edges {
            dst_set.insert(dst_p);
        }
        for p in dst_set {
            for t in 0..t_len {
                let mut terms = vec![(var_idx(p, t, SLOT_R, t_len), 1.0)];
                for (e_idx, &(_, dst_p, _, _)) in model.transport_edges.iter().enumerate() {
                    if dst_p == p {
                        terms.push((tr_base + e_idx * t_len + t, -1.0));
                    }
                }
                eq_rows.push(SparseRow {
                    terms,
                    rhs: 0.0,
                    bottleneck_desc: None,
                });
            }
        }

        num_vars += n_tr;
    }

    // 6. Plan-stability penalty `λ × Σ(|P − P_prev| + |R − R_prev|)`
    let lam = cfg.plan_stability_lambda;
    if lam > 0.0 {
        let d_base = num_vars;
        let n_d = n * t_len * 2;
        bounds.resize(d_base + n_d, (0.0, f64::INFINITY));
        c.resize(d_base + n_d, 0.0);

        for (p, pair) in model.all_pairs.iter().enumerate() {
            for t in 0..t_len {
                let row = model.facts_lookup.get(&(
                    pair.location.clone(),
                    pair.sku.clone(),
                    model.weeks[t].clone(),
                ));
                let d_p = d_base + p * t_len + t;
                let d_r = d_base + n * t_len + p * t_len + t;
                if pair.can_produce {
                    c[d_p] = lam;
                    let prev = row.map(|r| r.production_qty as f64).unwrap_or(0.0);
                    let x_var = var_idx(p, t, SLOT_P, t_len);
                    ub_rows.push(SparseRow {
                        terms: vec![(x_var, 1.0), (d_p, -1.0)],
                        rhs: prev,
                        bottleneck_desc: None,
                    });
                    ub_rows.push(SparseRow {
                        terms: vec![(x_var, -1.0), (d_p, -1.0)],
                        rhs: -prev,
                        bottleneck_desc: None,
                    });
                } else {
                    bounds[d_p] = (0.0, 0.0);
                }
                if pair.can_replenish {
                    c[d_r] = lam;
                    let prev = row.map(|r| r.replenishment_qty as f64).unwrap_or(0.0);
                    let x_var = var_idx(p, t, SLOT_R, t_len);
                    ub_rows.push(SparseRow {
                        terms: vec![(x_var, 1.0), (d_r, -1.0)],
                        rhs: prev,
                        bottleneck_desc: None,
                    });
                    ub_rows.push(SparseRow {
                        terms: vec![(x_var, -1.0), (d_r, -1.0)],
                        rhs: -prev,
                        bottleneck_desc: None,
                    });
                } else {
                    bounds[d_r] = (0.0, 0.0);
                }
            }
        }
        num_vars += n_d;
    }

    let line_mode: HashMap<(String, String), (bool, f64, f64)> = line_time_based
        .iter()
        .map(|(k, &tb)| {
            let hours = model.lookups.line_hours.get(k).copied().unwrap_or(0.0);
            let max_cap = model.lookups.line_capacity.get(k).copied().unwrap_or(0.0);
            (k.clone(), (tb, hours, max_cap))
        })
        .collect();

    FormulatedLp {
        num_vars,
        c,
        bounds,
        eq_rows,
        ub_rows,
        pa_base,
        line_row_idx,
        line_mode,
    }
}
