use crate::graph::{
    assert_uniform_weekly_axis, bucket_receipts_by_week, compute_safety_stock_target,
    propagate_downstream_demand,
};
use crate::lookups::{build_capacity_calendar, build_master_lookups, CapacityCalendar, MasterLookups};
use crate::schema::{FactRow, OptimizeRequestPayload, OptimizerError, SolverConfig};
use std::collections::{BTreeSet, HashMap, HashSet};

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
    /// `supplier_p -> Vec<(dst_p, trans_lt, mult)>`
    pub transport_consumers: HashMap<usize, Vec<(usize, i64, f64)>>,
    /// `Vec<(src_p, dst_p, trans_lt, cost)>` for Network Design formulation
    pub transport_edges: Vec<(usize, usize, i64, f64)>,
    pub has_quota_split: bool,
    pub supply_p_lt: HashMap<usize, i64>,
    pub supply_r_lt: HashMap<usize, i64>,
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
    );

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

    let n = all_pairs.len();
    if n == 0 {
        return Ok(None);
    }

    // BOM adjacency
    let mut bom_consumers: HashMap<usize, Vec<(usize, f64, i64)>> = HashMap::new();
    for r in &lookups.bom_list {
        let src = (r.source_location.clone(), r.child_sku.clone());
        let parent = (r.target_location.clone(), r.parent_sku.clone());
        if let (Some(&src_p), Some(&parent_p)) = (pair_idx_map.get(&src), pair_idx_map.get(&parent))
        {
            let prod_lt = *lookups.prod_lt_map.get(&parent).unwrap_or(&0);
            let scrap = r.scrap_rate;
            let raw_qty = r.quantity;
            let effective_qty = if scrap < 1.0 {
                raw_qty / (1.0 - scrap)
            } else {
                raw_qty
            };
            bom_consumers
                .entry(src_p)
                .or_default()
                .push((parent_p, effective_qty, prod_lt));
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
    for entries in quota_groups.values() {
        if entries.len() >= 2 && entries.iter().all(|(_, p)| p.is_some()) {
            let total: f64 = entries.iter().filter_map(|(_, p)| *p).sum();
            let denom = if total > 0.0 { total } else { 1.0 };
            for &(r_idx, p_opt) in entries {
                route_multiplier[r_idx] = p_opt.unwrap_or(0.0) / denom;
            }
            has_quota_split = true;
        } else {
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
    for (r_idx, r) in lookups.transport_list.iter().enumerate() {
        let src = (r.from_location.clone(), r.sku.clone());
        let dst = (r.to_location.clone(), r.sku.clone());
        if let (Some(&src_p), Some(&dst_p)) = (pair_idx_map.get(&src), pair_idx_map.get(&dst)) {
            let trans_lt = r.transport_lt.unwrap_or(0);
            let cost = r.transport_cost_per_unit.unwrap_or(0.0);
            let mult = route_multiplier[r_idx];
            if mult <= 0.0 {
                continue;
            }
            transport_edges.push((src_p, dst_p, trans_lt, cost));
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

    propagate_downstream_demand(
        &mut demand_for_target,
        &bom_consumers,
        &transport_consumers,
        n,
        t_len,
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
        transport_consumers,
        transport_edges,
        has_quota_split,
        supply_p_lt,
        supply_r_lt,
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
}

/// Formulate the full LP (variables, bounds, objective, equality and inequality constraints,
/// Network Design `TR` edges, and plan-stability `D` variables).
pub fn formulate_lp(model: &LpModel, cfg: &SolverConfig) -> FormulatedLp {
    let n = model.n;
    let t_len = model.t_len;
    let base_vars = n * t_len * V_SLOTS;
    let mut bounds = vec![(0.0, f64::INFINITY); base_vars];

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

            if !pair.can_produce {
                bounds[var_idx(p, t, SLOT_P, t_len)] = (0.0, 0.0);
            } else if t_i64 < p_fence {
                let p_qty = row.map(|r| r.production_qty as f64).unwrap_or(0.0);
                bounds[var_idx(p, t, SLOT_P, t_len)] = (p_qty, p_qty);
            } else if t_i64 < p_lt_val {
                bounds[var_idx(p, t, SLOT_P, t_len)] = (0.0, 0.0);
            }

            if !pair.can_replenish {
                bounds[var_idx(p, t, SLOT_R, t_len)] = (0.0, 0.0);
            } else if t_i64 < r_fence {
                let r_qty = row.map(|r| r.replenishment_qty as f64).unwrap_or(0.0);
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
    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let route_cost = *model.lookups.transport_cost.get(&key).unwrap_or(&0.0);
        let replen_cost = if route_cost > 0.0 {
            route_cost
        } else {
            cfg.replenishment_cost
        };
        let sku_cost = model.lookups.cost_map.get(&key).copied();
        let hr = model.lookups.location_holding_rate.get(&pair.location).copied();
        let hc = match (hr, sku_cost) {
            (Some(rate), Some(sc)) => rate * sc,
            _ => *model
                .lookups
                .location_holding_cost
                .get(&pair.location)
                .unwrap_or(&cfg.holding_cost),
        };
        let prod_cost = sku_cost.unwrap_or(cfg.production_cost);
        let procure_cost = model
            .lookups
            .procurement_cost_map
            .get(&key)
            .copied()
            .or(sku_cost)
            .unwrap_or(cfg.procurement_cost);
        let sku_price = model.lookups.price_map.get(&key).copied();
        let shortage_cost = if cfg.profit_maximization && sku_price.is_some() {
            sku_price.unwrap()
        } else if let Some(sp) = sku_price {
            sp.max(cfg.shortage_penalty)
        } else {
            cfg.shortage_penalty
        };

        for t in 0..t_len {
            c[var_idx(p, t, SLOT_I, t_len)] = hc;
            if model.backorder_pairs.contains(&p) {
                c[var_idx(p, t, SLOT_U, t_len)] = cfg.backlog_penalty_per_week
                    + if t == t_len - 1 { shortage_cost } else { 0.0 };
            } else {
                c[var_idx(p, t, SLOT_U, t_len)] = shortage_cost;
            }
            c[var_idx(p, t, SLOT_E, t_len)] = procure_cost;
            c[var_idx(p, t, SLOT_SS, t_len)] = cfg.ss_violation_penalty;
            if pair.can_produce {
                c[var_idx(p, t, SLOT_P, t_len)] = prod_cost;
            }
            if pair.can_replenish {
                c[var_idx(p, t, SLOT_R, t_len)] = replen_cost;
            }
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
                for &(parent_p, bom_qty, prod_lt) in bom_edges {
                    let lt_u = prod_lt.max(0) as usize;
                    if t == 0 {
                        let max_tp = lt_u.min(t_len - 1);
                        for t_parent in 0..=max_tp {
                            *coeff_map
                                .entry(var_idx(parent_p, t_parent, SLOT_P, t_len))
                                .or_insert(0.0) -= bom_qty;
                        }
                    } else {
                        let t_parent = t + lt_u;
                        if t_parent < t_len {
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

        for t in 0..t_len {
            let w = &model.weeks[t];
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

    for loc in unassigned_order {
        let pairs = &unassigned_pairs[&loc];
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
                    let terms: Vec<(usize, f64)> = pairs
                        .iter()
                        .map(|&p| (var_idx(p, t, SLOT_I, t_len), 1.0))
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

    FormulatedLp {
        num_vars,
        c,
        bounds,
        eq_rows,
        ub_rows,
    }
}
