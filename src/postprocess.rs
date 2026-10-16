use crate::graph::supplier_to_consumer_order;
use crate::lookups::apply_lot_sizing;
use crate::lp_model::{var_idx, LpModel, SLOT_E, SLOT_I, SLOT_P, SLOT_R};
use crate::schema::{FactRow, OptimizeRequestPayload, PlanEditRow};
use std::collections::HashMap;

pub type PairWeekKey = (usize, usize);

/// Round the LP's continuous P/R/E variables UP to their lot sizes (`solver._round_lp_solution`).
pub fn round_lp_solution(
    x: &[f64],
    model: &LpModel,
) -> (
    HashMap<PairWeekKey, i64>,
    HashMap<PairWeekKey, i64>,
    HashMap<PairWeekKey, i64>,
) {
    let t_len = model.t_len;
    let mut prod_raw: HashMap<PairWeekKey, i64> = HashMap::with_capacity(model.n * t_len);
    let mut replen_out: HashMap<PairWeekKey, i64> = HashMap::with_capacity(model.n * t_len);
    let mut release_out: HashMap<PairWeekKey, i64> = HashMap::with_capacity(model.n * t_len);

    // Note: in Python `max(0.0, x[idx])`, interior-point solvers (Clarabel) may have tiny noise
    // around 0 (e.g. 1e-11) on zero variables. Clamp values <= 1e-7 to 0.0 so a zero variable
    // does not get rounded up to MOQ/lot_size.
    const ZERO_TOL: f64 = 1e-7;

    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let (p_min, p_lot) = *model
            .lookups
            .production_lot_map
            .get(&key)
            .unwrap_or(&(0, 1));
        let (s_min, s_lot) = *model.lookups.shipping_lot_map.get(&key).unwrap_or(&(0, 1));
        let (r_min, r_lot) = *model
            .lookups
            .procurement_lot_map
            .get(&key)
            .unwrap_or(&(0, 1));
        let e_lt = *model.lookups.procurement_lt_map.get(&key).unwrap_or(&0);

        for t in 0..t_len {
            let p_val = x[var_idx(p, t, SLOT_P, t_len)];
            let e_val = x[var_idx(p, t, SLOT_E, t_len)];
            let prod = if p_val > ZERO_TOL { p_val } else { 0.0 };
            let ext = if e_val > ZERO_TOL { e_val } else { 0.0 };
            prod_raw.insert((p, t), apply_lot_sizing(prod, p_min, p_lot));
            release_out.insert((p, t), apply_lot_sizing(ext, r_min, r_lot));
        }

        for t in 0..t_len {
            let r_val = x[var_idx(p, t, SLOT_R, t_len)];
            let replen = if r_val > ZERO_TOL { r_val } else { 0.0 };
            let received = if (t as i64) - e_lt >= 0 {
                let t_order = ((t as i64) - e_lt) as usize;
                *release_out.get(&(p, t_order)).unwrap_or(&0)
            } else {
                0
            };
            let shipped = apply_lot_sizing(replen, s_min, s_lot);
            replen_out.insert((p, t), shipped + received);
        }
    }

    (prod_raw, replen_out, release_out)
}

/// Clip rounded production DOWN to respect per-line / per-location capacity (`solver_base._clip_by_capacity`).
pub fn clip_by_capacity(prod_raw: &mut HashMap<PairWeekKey, i64>, model: &LpModel) {
    let t_len = model.t_len;
    let mut line_order: Vec<(String, String)> = Vec::new();
    let mut line_pairs: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut loc_order: Vec<String> = Vec::new();
    let mut loc_pairs: HashMap<String, Vec<usize>> = HashMap::new();

    for (p, pair) in model.all_pairs.iter().enumerate() {
        if !pair.can_produce {
            continue;
        }
        let key = (pair.location.clone(), pair.sku.clone());
        if let Some(line) = model.lookups.sku_to_line.get(&key) {
            let lk = (pair.location.clone(), line.clone());
            if !line_pairs.contains_key(&lk) {
                line_order.push(lk.clone());
            }
            line_pairs.entry(lk).or_default().push(p);
        } else if model.lookups.location_capacity.contains_key(&pair.location) {
            if !loc_pairs.contains_key(&pair.location) {
                loc_order.push(pair.location.clone());
            }
            loc_pairs.entry(pair.location.clone()).or_default().push(p);
        }
    }

    let clip_group = |prod_raw: &mut HashMap<PairWeekKey, i64>,
                      pairs: &[usize],
                      cap_for_t: &dyn Fn(usize) -> i64| {
        for t in 0..t_len {
            let cap_w = cap_for_t(t);
            let total: i64 = pairs
                .iter()
                .map(|&p| *prod_raw.get(&(p, t)).unwrap_or(&0))
                .sum();
            if total <= cap_w {
                continue;
            }
            let mut ordered: Vec<usize> = pairs.to_vec();
            ordered.sort_by(|&pa, &pb| {
                let ka = (
                    model.all_pairs[pa].location.clone(),
                    model.all_pairs[pa].sku.clone(),
                );
                let kb = (
                    model.all_pairs[pb].location.clone(),
                    model.all_pairs[pb].sku.clone(),
                );
                let ma = *model.lookups.margin_map.get(&ka).unwrap_or(&0.0);
                let mb = *model.lookups.margin_map.get(&kb).unwrap_or(&0.0);
                let qa = *prod_raw.get(&(pa, t)).unwrap_or(&0);
                let qb = *prod_raw.get(&(pb, t)).unwrap_or(&0);
                mb.partial_cmp(&ma)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| qb.cmp(&qa))
                    .then_with(|| pa.cmp(&pb))
            });
            let mut remaining = cap_w;
            for p in ordered {
                let cur = *prod_raw.get(&(p, t)).unwrap_or(&0);
                if cur == 0 {
                    continue;
                }
                let key = (
                    model.all_pairs[p].location.clone(),
                    model.all_pairs[p].sku.clone(),
                );
                let (moq, lot_raw) = *model
                    .lookups
                    .production_lot_map
                    .get(&key)
                    .unwrap_or(&(0, 1));
                let lot = lot_raw.max(1);
                let allowed = if remaining > 0 {
                    (remaining / lot) * lot
                } else {
                    0
                };
                let mut clipped = if allowed <= 0 { 0 } else { cur.min(allowed) };
                if clipped > 0 && clipped < moq {
                    clipped = 0;
                }
                prod_raw.insert((p, t), clipped);
                remaining -= clipped;
            }
        }
    };

    for (loc, line) in line_order {
        let pairs = &line_pairs[&(loc.clone(), line.clone())];
        let lk = (loc.clone(), line.clone());
        let hours = *model.lookups.line_hours.get(&lk).unwrap_or(&0.0);
        let all_have_rate = hours > 0.0
            && pairs.iter().all(|&p| {
                let k = (
                    model.all_pairs[p].location.clone(),
                    model.all_pairs[p].sku.clone(),
                );
                model.lookups.sku_production_rate.contains_key(&k)
            });

        if all_have_rate {
            let mut per_unit: HashMap<usize, f64> = HashMap::new();
            for &p in pairs {
                let k = (
                    model.all_pairs[p].location.clone(),
                    model.all_pairs[p].sku.clone(),
                );
                let info = &model.lookups.sku_production_rate[&k];
                per_unit.insert(p, 1.0 / (info.rate_per_hour * info.yield_rate));
            }
            for t in 0..t_len {
                let avail = *model
                    .calendar
                    .hours_t
                    .get(&(loc.clone(), line.clone(), t))
                    .unwrap_or(&hours);
                let total_hours: f64 = pairs
                    .iter()
                    .map(|&p| (*prod_raw.get(&(p, t)).unwrap_or(&0) as f64) * per_unit[&p])
                    .sum();
                if total_hours <= avail + 1e-9 {
                    continue;
                }
                let mut ordered: Vec<usize> = pairs.clone();
                ordered.sort_by(|&pa, &pb| {
                    let ka = (
                        model.all_pairs[pa].location.clone(),
                        model.all_pairs[pa].sku.clone(),
                    );
                    let kb = (
                        model.all_pairs[pb].location.clone(),
                        model.all_pairs[pb].sku.clone(),
                    );
                    let ma = *model.lookups.margin_map.get(&ka).unwrap_or(&0.0);
                    let mb = *model.lookups.margin_map.get(&kb).unwrap_or(&0.0);
                    let qa = *prod_raw.get(&(pa, t)).unwrap_or(&0);
                    let qb = *prod_raw.get(&(pb, t)).unwrap_or(&0);
                    mb.partial_cmp(&ma)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| qb.cmp(&qa))
                        .then_with(|| pa.cmp(&pb))
                });
                let mut remaining = avail;
                for p in ordered {
                    let cur = *prod_raw.get(&(p, t)).unwrap_or(&0);
                    if cur == 0 {
                        continue;
                    }
                    let key = (
                        model.all_pairs[p].location.clone(),
                        model.all_pairs[p].sku.clone(),
                    );
                    let (moq, lot_raw) = *model
                        .lookups
                        .production_lot_map
                        .get(&key)
                        .unwrap_or(&(0, 1));
                    let lot = lot_raw.max(1);
                    let fits = if remaining > 0.0 {
                        ((remaining / per_unit[&p]) + 1e-9).floor() as i64
                    } else {
                        0
                    };
                    let allowed = (fits / lot) * lot;
                    let mut clipped = if allowed <= 0 { 0 } else { cur.min(allowed) };
                    if clipped > 0 && clipped < moq {
                        clipped = 0;
                    }
                    prod_raw.insert((p, t), clipped);
                    remaining -= (clipped as f64) * per_unit[&p];
                }
            }
            continue;
        }

        if let Some(&cap) = model.lookups.line_capacity.get(&lk) {
            let static_cap = cap as i64;
            clip_group(prod_raw, pairs, &|t| {
                model
                    .calendar
                    .line_cap_t
                    .get(&(loc.clone(), line.clone(), t))
                    .map(|&v| v as i64)
                    .unwrap_or(static_cap)
            });
        }
    }

    for loc in loc_order {
        let pairs = &loc_pairs[&loc];
        if let Some(&cap) = model.lookups.location_capacity.get(&loc) {
            let static_cap = cap as i64;
            clip_group(prod_raw, pairs, &|t| {
                model
                    .calendar
                    .loc_cap_t
                    .get(&(loc.clone(), t))
                    .map(|&v| v as i64)
                    .unwrap_or(static_cap)
            });
        }
    }
}

/// PMU-9b: a source cannot ship more than it has (`solver_base._clip_transport_to_supply`).
pub fn clip_transport_to_supply(
    prod_raw: &HashMap<PairWeekKey, i64>,
    replen_out: &mut HashMap<PairWeekKey, i64>,
    ship_out: &mut HashMap<PairWeekKey, i64>,
    x: &[f64],
    model: &LpModel,
) {
    let t_len = model.t_len;
    let mut order = supplier_to_consumer_order(
        model.n,
        &model.bom_consumers,
        &model.transport_consumers,
    );
    order.reverse();

    let weeks_drawn = |t: usize, lt_raw: i64| -> Vec<usize> {
        let lt = lt_raw.max(0) as usize;
        if t == 0 {
            (0..=lt.min(t_len - 1)).collect()
        } else if t + lt < t_len {
            vec![t + lt]
        } else {
            Vec::new()
        }
    };

    const ZERO_TOL: f64 = 1e-7;
    let lp_val = |p: usize, t: usize, slot: usize| -> f64 {
        let v = x[var_idx(p, t, slot, t_len)];
        if v > ZERO_TOL { v } else { 0.0 }
    };

    for p in order {
        let Some(drains) = model.transport_consumers.get(&p) else {
            continue;
        };
        if drains.is_empty() {
            continue;
        }
        let key = (
            model.all_pairs[p].location.clone(),
            model.all_pairs[p].sku.clone(),
        );
        let e_lt = *model.lookups.procurement_lt_map.get(&key).unwrap_or(&0);
        let mut cum = 0.0;

        for t in 0..t_len {
            let p_disc = *prod_raw.get(&(p, t)).unwrap_or(&0) as f64;
            let s_disc = *ship_out.get(&(p, t)).unwrap_or(&0) as f64;
            let r_disc = *replen_out.get(&(p, t)).unwrap_or(&0) as f64;

            let mut d_supply =
                (p_disc - lp_val(p, t, SLOT_P)) + (s_disc - lp_val(p, t, SLOT_R));
            d_supply += r_disc - s_disc;
            if (t as i64) - e_lt >= 0 {
                let te = ((t as i64) - e_lt) as usize;
                d_supply -= lp_val(p, te, SLOT_E);
            }

            let mut d_bom = 0.0;
            if let Some(bom_edges) = model.bom_consumers.get(&p) {
                for &(parent_p, bom_qty, prod_lt) in bom_edges {
                    for tp in weeks_drawn(t, prod_lt) {
                        let parent_disc = *prod_raw.get(&(parent_p, tp)).unwrap_or(&0) as f64;
                        d_bom += bom_qty * (parent_disc - lp_val(parent_p, tp, SLOT_P));
                    }
                }
            }

            let mut legs: Vec<(usize, usize, f64)> = Vec::new();
            for &(dst_p, trans_lt, mult) in drains {
                for td in weeks_drawn(t, trans_lt) {
                    legs.push((dst_p, td, mult));
                }
            }

            let d_out: f64 = legs
                .iter()
                .map(|&(dst_p, td, mult)| {
                    let dst_ship = *ship_out.get(&(dst_p, td)).unwrap_or(&0) as f64;
                    mult * (dst_ship - lp_val(dst_p, td, SLOT_R))
                })
                .sum();

            cum += d_supply - d_bom - d_out;
            let mut deficit = -(lp_val(p, t, SLOT_I) + cum);
            if deficit > 1e-6 {
                let mut sorted_legs = legs.clone();
                sorted_legs.sort_by(|a, b| {
                    let sa = *ship_out.get(&(a.0, a.1)).unwrap_or(&0);
                    let sb = *ship_out.get(&(b.0, b.1)).unwrap_or(&0);
                    sb.cmp(&sa)
                });
                for (dst_p, td, mult) in sorted_legs {
                    if deficit <= 1e-6 {
                        break;
                    }
                    let shipped = *ship_out.get(&(dst_p, td)).unwrap_or(&0);
                    if shipped <= 0 || mult <= 0.0 {
                        continue;
                    }
                    let d_key = (
                        model.all_pairs[dst_p].location.clone(),
                        model.all_pairs[dst_p].sku.clone(),
                    );
                    let (s_min, s_lot) =
                        *model.lookups.shipping_lot_map.get(&d_key).unwrap_or(&(0, 1));
                    let lot = s_lot.max(1);
                    let keep_raw = ((shipped as f64) - deficit / mult).max(0.0);
                    let mut keep = (((keep_raw + 1e-9).floor() as i64) / lot) * lot;
                    if keep > 0 && keep < s_min {
                        keep = 0;
                    }
                    let cut = shipped - keep;
                    ship_out.insert((dst_p, td), keep);
                    if let Some(r_val) = replen_out.get_mut(&(dst_p, td)) {
                        *r_val -= cut;
                    }
                    deficit -= (cut as f64) * mult;
                    cum += (cut as f64) * mult;
                }
                if deficit > 1e-6 {
                    cum += deficit;
                }
            }
        }
    }
}

/// L-10: shave rounding-induced excess production lot by lot (`solver_base._clip_excess_inventory`).
pub fn clip_excess_inventory(
    prod_raw: &mut HashMap<PairWeekKey, i64>,
    replen_out: &HashMap<PairWeekKey, i64>,
    ship_out: &HashMap<PairWeekKey, i64>,
    model: &LpModel,
    default_buffer_ratio: f64,
) {
    let t_len = model.t_len;
    let order = supplier_to_consumer_order(
        model.n,
        &model.bom_consumers,
        &model.transport_consumers,
    );

    for p in order {
        let pair = &model.all_pairs[p];
        if !pair.can_produce {
            continue;
        }
        let key = (pair.location.clone(), pair.sku.clone());
        let (p_moq, p_lot) = *model
            .lookups
            .production_lot_map
            .get(&key)
            .unwrap_or(&(0, 1));
        if p_lot <= 1 {
            continue;
        }
        let pair_ratio = *model
            .lookups
            .buffer_ratio_map
            .get(&key)
            .unwrap_or(&default_buffer_ratio);
        let mut inv = *model.lookups.init_inv_map.get(&key).unwrap_or(&0.0);

        for t in 0..t_len {
            let w = &model.weeks[t];
            let sales = model
                .facts_lookup
                .get(&(pair.location.clone(), pair.sku.clone(), w.clone()))
                .map(|r| r.sales_qty)
                .unwrap_or(0.0);

            let mut transport_out = 0.0;
            if let Some(tr_edges) = model.transport_consumers.get(&p) {
                for &(dst_p, trans_lt, quota) in tr_edges {
                    let lt = trans_lt.max(0) as usize;
                    if t == 0 {
                        for t_dst in 0..=lt.min(t_len - 1) {
                            transport_out +=
                                quota * (*ship_out.get(&(dst_p, t_dst)).unwrap_or(&0) as f64);
                        }
                    } else if t + lt < t_len {
                        transport_out +=
                            quota * (*ship_out.get(&(dst_p, t + lt)).unwrap_or(&0) as f64);
                    }
                }
            }

            let mut bom_out = 0.0;
            if let Some(bom_edges) = model.bom_consumers.get(&p) {
                for &(parent_p, bom_qty, prod_lt) in bom_edges {
                    let lt = prod_lt.max(0) as usize;
                    if t == 0 {
                        for t_parent in 0..=lt.min(t_len - 1) {
                            bom_out +=
                                bom_qty * (*prod_raw.get(&(parent_p, t_parent)).unwrap_or(&0) as f64);
                        }
                    } else if t + lt < t_len {
                        bom_out +=
                            bom_qty * (*prod_raw.get(&(parent_p, t + lt)).unwrap_or(&0) as f64);
                    }
                }
            }

            let mut prod = *prod_raw.get(&(p, t)).unwrap_or(&0);
            let mut replen = *replen_out.get(&(p, t)).unwrap_or(&0) as f64;
            if let Some(ref rec) = model.receipts {
                replen += rec[p][t];
            }

            let target = model.target_inv[p][t];
            if target > 0.0 {
                let ceiling = target * pair_ratio;
                let lot_f = p_lot as f64;
                while prod >= p_lot {
                    let projected =
                        inv + (prod as f64) + replen - sales - transport_out - bom_out;
                    if projected <= ceiling {
                        break;
                    }
                    if projected - lot_f < 0.0 {
                        break;
                    }
                    if prod - p_lot < p_moq {
                        if inv + replen - sales - transport_out - bom_out >= 0.0 {
                            prod = 0;
                        }
                        break;
                    }
                    prod -= p_lot;
                }
            }
            prod_raw.insert((p, t), prod);
            inv = inv + (prod as f64) + replen - sales - transport_out - bom_out;
            if inv < 0.0 {
                inv = 0.0;
            }
        }
    }
}

pub struct PostprocessOutput {
    pub updated_facts: Vec<FactRow>,
    pub edits: Vec<PlanEditRow>,
    pub prod_raw: HashMap<PairWeekKey, i64>,
    pub replen_out: HashMap<PairWeekKey, i64>,
    pub release_out: HashMap<PairWeekKey, i64>,
    pub ship_out: HashMap<PairWeekKey, i64>,
}

/// Full post-processing pipeline returning intermediate clipped maps for analytics.
pub fn extract_postprocess_output(
    x: &[f64],
    model: &LpModel,
    req: &OptimizeRequestPayload,
) -> PostprocessOutput {
    let t_len = model.t_len;
    let (mut prod_raw, mut replen_out, release_out) = round_lp_solution(x, model);

    clip_by_capacity(&mut prod_raw, model);

    // PMU-9b shipping clip
    let mut ship_out: HashMap<PairWeekKey, i64> = HashMap::with_capacity(model.n * t_len);
    const ZERO_TOL: f64 = 1e-7;
    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let (s_min, s_lot) = *model.lookups.shipping_lot_map.get(&key).unwrap_or(&(0, 1));
        for t in 0..t_len {
            let r_val = x[var_idx(p, t, SLOT_R, t_len)];
            let r_pos = if r_val > ZERO_TOL { r_val } else { 0.0 };
            ship_out.insert((p, t), apply_lot_sizing(r_pos, s_min, s_lot));
        }
    }

    clip_transport_to_supply(&prod_raw, &mut replen_out, &mut ship_out, x, model);
    clip_excess_inventory(&mut prod_raw, &replen_out, &ship_out, model, 1.2);

    // Build lookup maps for write-back
    let mut opt_map: HashMap<(String, String, String), (i64, i64, i64)> =
        HashMap::with_capacity(model.n * t_len);
    for (p, pair) in model.all_pairs.iter().enumerate() {
        for (t, w) in model.weeks.iter().enumerate() {
            opt_map.insert(
                (pair.location.clone(), pair.sku.clone(), w.clone()),
                (
                    *prod_raw.get(&(p, t)).unwrap_or(&0),
                    *replen_out.get(&(p, t)).unwrap_or(&0),
                    *release_out.get(&(p, t)).unwrap_or(&0),
                ),
            );
        }
    }

    let req_map: HashMap<(String, String, String), f64> = req
        .request_map
        .iter()
        .map(|e| ((e.location.clone(), e.sku.clone(), e.week_start.clone()), e.qty))
        .collect();
    let req_kind_map: HashMap<(String, String), &str> = req
        .request_kind_map
        .iter()
        .map(|e| ((e.location.clone(), e.sku.clone()), e.kind.as_str()))
        .collect();

    let mut updated_facts = Vec::with_capacity(req.facts.len());
    let mut edits = Vec::new();

    for row in &req.facts {
        let key = (row.location.clone(), row.sku.clone(), row.week_start.clone());
        if let Some(&(prod, replen, release)) = opt_map.get(&key) {
            let mut prod_request = prod;
            let mut replen_request = replen;
            let ls_key = (row.location.clone(), row.sku.clone());
            if let (Some(&kind), Some(&req_val_f)) =
                (req_kind_map.get(&ls_key), req_map.get(&key))
            {
                let rounded = req_val_f.round() as i64;
                if kind == "production_qty" {
                    prod_request = rounded;
                } else {
                    replen_request = rounded;
                }
            }

            let orig_prod = row.production_qty;
            let orig_replen = row.replenishment_qty;
            let orig_release = row.order_release_qty.unwrap_or(0);
            if prod != orig_prod || replen != orig_replen || release != orig_release {
                edits.push(PlanEditRow {
                    location: row.location.clone(),
                    sku: row.sku.clone(),
                    week_start: row.week_start.clone(),
                    production_qty: prod,
                    replenishment_qty: replen,
                    production_request: prod_request,
                    replenishment_request: replen_request,
                    order_release_qty: release,
                });
            }

            let mut new_row = row.clone();
            new_row.production_qty = prod;
            new_row.replenishment_qty = replen;
            new_row.production_request = Some(prod_request);
            new_row.replenishment_request = Some(replen_request);
            new_row.order_release_qty = Some(release);
            updated_facts.push(new_row);
        } else {
            let mut new_row = row.clone();
            if new_row.order_release_qty.is_none() {
                new_row.order_release_qty = Some(0);
            }
            updated_facts.push(new_row);
        }
    }

    PostprocessOutput {
        updated_facts,
        edits,
        prod_raw,
        replen_out,
        release_out,
        ship_out,
    }
}

/// Full post-processing pipeline (`solver._extract_results` + `solver._write_back_to_facts`).
pub fn extract_and_write_back(
    x: &[f64],
    model: &LpModel,
    req: &OptimizeRequestPayload,
) -> (Vec<FactRow>, Vec<PlanEditRow>) {
    let out = extract_postprocess_output(x, model, req);
    (out.updated_facts, out.edits)
}

