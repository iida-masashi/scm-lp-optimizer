use std::collections::{BTreeSet, HashMap};

use crate::lp_model::{var_idx, LpModel, SLOT_U};
use crate::postprocess::PairWeekKey;
use crate::schema::{
    Bottleneck, CausalFactor, LineSkuBreakdown, LineUtilizationRow, OptimizeRequestPayload,
    RootCauseDiagnostic, SkuPsiDetailRow, TransportFlowRow, WhatIfAction,
};

fn round2(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

/// Compute weekly production line utilization rows and SKU breakdowns.
pub fn compute_line_utilizations(
    prod_raw: &HashMap<PairWeekKey, i64>,
    model: &LpModel,
    req: &OptimizeRequestPayload,
    bottlenecks: &[Bottleneck],
) -> Vec<LineUtilizationRow> {
    let t_len = model.t_len;
    let mut line_keys: Vec<(String, String)> = Vec::new();
    let mut seen_lines: BTreeSet<(String, String)> = BTreeSet::new();

    for l in &req.lines {
        let k = (l.location.clone(), l.line_id.clone());
        if seen_lines.insert(k.clone()) {
            line_keys.push(k);
        }
    }
    for (p, pair) in model.all_pairs.iter().enumerate() {
        if !pair.can_produce {
            continue;
        }
        let k = (pair.location.clone(), pair.sku.clone());
        if let Some(line_id) = model.lookups.sku_to_line.get(&k) {
            let lk = (pair.location.clone(), line_id.clone());
            if seen_lines.insert(lk.clone()) {
                line_keys.push(lk);
            }
        } else if model.lookups.location_capacity.contains_key(&pair.location) {
            let lk = (pair.location.clone(), "DEFAULT".to_string());
            if seen_lines.insert(lk.clone()) {
                line_keys.push(lk);
            }
        }
        let _ = p;
    }

    let mut line_to_pairs: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (p, pair) in model.all_pairs.iter().enumerate() {
        if !pair.can_produce {
            continue;
        }
        let k = (pair.location.clone(), pair.sku.clone());
        let line_id = model
            .lookups
            .sku_to_line
            .get(&k)
            .cloned()
            .unwrap_or_else(|| "DEFAULT".to_string());
        line_to_pairs
            .entry((pair.location.clone(), line_id))
            .or_default()
            .push(p);
    }

    let mut rows = Vec::new();

    for (loc, line_id) in line_keys {
        let lk = (loc.clone(), line_id.clone());
        let pairs = line_to_pairs.get(&lk).cloned().unwrap_or_default();
        let base_hours = *model.lookups.line_hours.get(&lk).unwrap_or(&0.0);
        let base_cap = *model
            .lookups
            .line_capacity
            .get(&lk)
            .or_else(|| model.lookups.location_capacity.get(&loc))
            .unwrap_or(&0.0);

        let use_time_based = base_hours > 0.0
            && !pairs.is_empty()
            && pairs.iter().all(|&p| {
                let sk = (
                    model.all_pairs[p].location.clone(),
                    model.all_pairs[p].sku.clone(),
                );
                model.lookups.sku_production_rate.contains_key(&sk)
            });

        for (t, w) in model.weeks.iter().enumerate() {
            let avail_hours = *model
                .calendar
                .hours_t
                .get(&(loc.clone(), line_id.clone(), t))
                .unwrap_or(&base_hours);
            let max_cap_qty = *model
                .calendar
                .line_cap_t
                .get(&(loc.clone(), line_id.clone(), t))
                .unwrap_or(&base_cap);

            let mut used_qty: i64 = 0;
            let mut used_hours: f64 = 0.0;
            let mut sku_items: Vec<(String, i64, f64)> = Vec::new();

            for &p in &pairs {
                let qty = *prod_raw.get(&(p, t)).unwrap_or(&0);
                let sku = model.all_pairs[p].sku.clone();
                let sk = (loc.clone(), sku.clone());
                let hrs = if let Some(rate_info) = model.lookups.sku_production_rate.get(&sk) {
                    let denom = (rate_info.rate_per_hour * rate_info.yield_rate).max(1e-9);
                    (qty as f64) / denom
                } else if max_cap_qty > 0.0 && avail_hours > 0.0 {
                    (qty as f64) * (avail_hours / max_cap_qty)
                } else {
                    0.0
                };
                used_qty += qty;
                used_hours += hrs;
                sku_items.push((sku, qty, hrs));
            }

            let util_pct = if use_time_based && avail_hours > 0.0 {
                (used_hours / avail_hours) * 100.0
            } else if max_cap_qty > 0.0 {
                ((used_qty as f64) / max_cap_qty) * 100.0
            } else {
                0.0
            };

            let time_desc = format!("Line Time Capacity at {loc} ({line_id}) in Week {w}");
            let qty_desc = format!("Line Qty Capacity at {loc} ({line_id}) in Week {w}");
            let loc_desc = format!("Location Capacity at {loc} in Week {w}");

            let shadow_price = bottlenecks
                .iter()
                .find(|b| {
                    b.description == time_desc
                        || b.description == qty_desc
                        || b.description == loc_desc
                })
                .map(|b| b.marginal_value.abs())
                .unwrap_or(0.0);

            let is_bottleneck = shadow_price > 1e-4 || util_pct >= 99.5;

            let mut sku_breakdown = Vec::with_capacity(sku_items.len());
            for (sku, q, h) in sku_items {
                let share = if use_time_based && avail_hours > 0.0 {
                    (h / avail_hours) * 100.0
                } else if max_cap_qty > 0.0 {
                    ((q as f64) / max_cap_qty) * 100.0
                } else {
                    0.0
                };
                sku_breakdown.push(LineSkuBreakdown {
                    sku,
                    production_qty: q,
                    used_hours: round2(h),
                    share_pct: round2(share),
                });
            }

            rows.push(LineUtilizationRow {
                location: loc.clone(),
                line_id: line_id.clone(),
                week_start: w.clone(),
                capacity_mode: if use_time_based {
                    "hours".to_string()
                } else {
                    "quantity".to_string()
                },
                used_quantity: used_qty,
                max_capacity_quantity: round2(max_cap_qty),
                used_hours: round2(used_hours),
                available_hours: round2(avail_hours),
                utilization_pct: round2(util_pct),
                is_bottleneck,
                shadow_price: round2(shadow_price),
                sku_breakdown,
            });
        }
    }

    rows
}

/// Compute route-level inter-location transport flows across all weeks.
pub fn compute_transport_flows(
    ship_out: &HashMap<PairWeekKey, i64>,
    model: &LpModel,
    req: &OptimizeRequestPayload,
) -> Vec<TransportFlowRow> {
    let t_len = model.t_len;
    let mut flows = Vec::new();

    // Group routes by destination (to_location, sku)
    let mut dst_routes: HashMap<(String, String), Vec<&crate::schema::TransportRouteRow>> =
        HashMap::new();
    for r in &req.transport_routes {
        dst_routes
            .entry((r.to_location.clone(), r.sku.clone()))
            .or_default()
            .push(r);
    }

    for (p, pair) in model.all_pairs.iter().enumerate() {
        let dst_key = (pair.location.clone(), pair.sku.clone());
        let Some(routes) = dst_routes.get(&dst_key) else {
            continue;
        };
        if routes.is_empty() {
            continue;
        }

        let total_quota: f64 = routes
            .iter()
            .map(|r| r.quota_pct.unwrap_or(0.0).max(0.0))
            .sum();

        for (t, arrival_w) in model.weeks.iter().enumerate() {
            let total_shipped_to_dst = *ship_out.get(&(p, t)).unwrap_or(&0);
            let mut allocated_sum = 0i64;

            for (idx, r) in routes.iter().enumerate() {
                let lt = r.transport_lt.unwrap_or(0).max(0);
                let ship_t = if (t as i64) >= lt {
                    (t as i64 - lt) as usize
                } else {
                    0
                };
                let ship_w = if (t as i64) >= lt {
                    model.weeks[ship_t].clone()
                } else {
                    format!("{} (期首/先行出荷)", model.weeks[0])
                };

                let route_qty = if routes.len() == 1 {
                    total_shipped_to_dst
                } else if idx + 1 == routes.len() {
                    (total_shipped_to_dst - allocated_sum).max(0)
                } else if total_quota > 1e-6 {
                    let share = r.quota_pct.unwrap_or(0.0).max(0.0) / total_quota;
                    let q = ((total_shipped_to_dst as f64) * share).round() as i64;
                    allocated_sum += q;
                    q
                } else {
                    let share = 1.0 / (routes.len() as f64);
                    let q = ((total_shipped_to_dst as f64) * share).round() as i64;
                    allocated_sum += q;
                    q
                };

                let unit_cost = r.transport_cost_per_unit.unwrap_or(0.0);
                flows.push(TransportFlowRow {
                    from_location: r.from_location.clone(),
                    to_location: r.to_location.clone(),
                    sku: r.sku.clone(),
                    ship_week: ship_w,
                    arrival_week: arrival_w.clone(),
                    transport_lt: lt,
                    qty: route_qty,
                    unit_cost: round2(unit_cost),
                    total_cost: round2((route_qty as f64) * unit_cost),
                });
            }
        }
    }

    flows
}

/// Compute SKU-level weekly PSI (Opening Inv, Production, Transport In, Procurement In,
/// Scheduled Receipts, Sales, BOM Demand, Transport Out, Ending Inv, Safety Stock, Shortage).
pub fn compute_sku_psi_details(
    prod_raw: &HashMap<PairWeekKey, i64>,
    replen_out: &HashMap<PairWeekKey, i64>,
    release_out: &HashMap<PairWeekKey, i64>,
    ship_out: &HashMap<PairWeekKey, i64>,
    x: &[f64],
    model: &LpModel,
    req: &OptimizeRequestPayload,
) -> Vec<SkuPsiDetailRow> {
    let t_len = model.t_len;
    let mut rows = Vec::with_capacity(model.n * t_len);

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

    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let e_lt = *model.lookups.procurement_lt_map.get(&key).unwrap_or(&0);
        let line_id = model.lookups.sku_to_line.get(&key).cloned();
        let mut opening_inv = *model.lookups.init_inv_map.get(&key).unwrap_or(&0.0);

        for (t, w) in model.weeks.iter().enumerate() {
            let prod_qty = *prod_raw.get(&(p, t)).unwrap_or(&0);
            let order_release = *release_out.get(&(p, t)).unwrap_or(&0);
            let procurement_in = if pair.can_procure && (t as i64) - e_lt >= 0 {
                let t_order = ((t as i64) - e_lt) as usize;
                *release_out.get(&(p, t_order)).unwrap_or(&0)
            } else {
                0
            };
            let total_replen = *replen_out.get(&(p, t)).unwrap_or(&0);
            let transport_in = (total_replen - procurement_in).max(0);

            let sr_qty = model
                .receipts
                .as_ref()
                .map(|r| r[p][t])
                .unwrap_or(0.0);

            let sales_qty = model
                .facts_lookup
                .get(&(pair.location.clone(), pair.sku.clone(), w.clone()))
                .map(|f| f.sales_qty)
                .unwrap_or(0.0);

            // Compute BOM dependent consumption drawn in week t
            let mut bom_demand = 0.0;
            if let Some(bom_edges) = model.bom_consumers.get(&p) {
                for &(parent_p, bom_qty, prod_lt) in bom_edges {
                    let lt = prod_lt.max(0) as usize;
                    if t == 0 {
                        for t_parent in 0..=lt.min(t_len - 1) {
                            bom_demand +=
                                bom_qty * (*prod_raw.get(&(parent_p, t_parent)).unwrap_or(&0) as f64);
                        }
                    } else if t + lt < t_len {
                        bom_demand +=
                            bom_qty * (*prod_raw.get(&(parent_p, t + lt)).unwrap_or(&0) as f64);
                    }
                }
            }

            // Compute downstream transport outbound drawn from this location in week t
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

            let mut prod_request = prod_qty;
            if let (Some(&kind), Some(&req_val_f)) = (
                req_kind_map.get(&key),
                req_map.get(&(pair.location.clone(), pair.sku.clone(), w.clone())),
            ) {
                if kind == "production_qty" {
                    prod_request = req_val_f.round() as i64;
                }
            }

            let total_supply =
                opening_inv + (prod_qty as f64) + (total_replen as f64) + sr_qty;
            let total_demand = sales_qty + bom_demand + transport_out;
            let net_balance = total_supply - total_demand;

            let (ending_inv, shortage_qty) = if net_balance >= 0.0 {
                (round2(net_balance), 0.0)
            } else {
                (0.0, round2(-net_balance))
            };

            let lp_shortage = {
                let u_val = x[var_idx(p, t, SLOT_U, t_len)];
                if u_val > 1e-5 {
                    round2(u_val)
                } else {
                    0.0
                }
            };
            let effective_shortage = shortage_qty.max(lp_shortage);

            let ss_target = round2(model.target_inv[p][t]);
            let ss_deficit = if ss_target > ending_inv {
                round2(ss_target - ending_inv)
            } else {
                0.0
            };

            rows.push(SkuPsiDetailRow {
                location: pair.location.clone(),
                sku: pair.sku.clone(),
                week_start: w.clone(),
                opening_inv: round2(opening_inv),
                production_qty: prod_qty,
                production_request: prod_request,
                transport_in_qty: transport_in,
                procurement_in_qty: procurement_in,
                order_release_qty: order_release,
                scheduled_receipt_qty: round2(sr_qty),
                sales_demand_qty: round2(sales_qty),
                bom_demand_qty: round2(bom_demand),
                transport_out_qty: round2(transport_out),
                ending_inv,
                safety_stock_target: ss_target,
                safety_stock_deficit: ss_deficit,
                shortage_qty: effective_shortage,
                lp_shortage_qty: lp_shortage,
                can_produce: pair.can_produce,
                can_replenish: pair.can_replenish,
                can_procure: pair.can_procure,
                line_id: line_id.clone(),
            });

            opening_inv = ending_inv;
        }
    }

    rows
}

/// Deterministic Upstream Root-Cause Tracer:
/// Explains why any shortage or safety-stock violation occurred by tracing upstream across
/// transport lead times, production lines, BOM components, procurement limits, and frozen fences.
pub fn compute_root_causes(
    psi_rows: &[SkuPsiDetailRow],
    line_utils: &[LineUtilizationRow],
    model: &LpModel,
    req: &OptimizeRequestPayload,
) -> Vec<RootCauseDiagnostic> {
    let mut diagnostics = Vec::new();

    let psi_map: HashMap<(String, String, String), &SkuPsiDetailRow> = psi_rows
        .iter()
        .map(|r| ((r.location.clone(), r.sku.clone(), r.week_start.clone()), r))
        .collect();

    let line_util_map: HashMap<(String, String, String), &LineUtilizationRow> = line_utils
        .iter()
        .map(|r| ((r.location.clone(), r.line_id.clone(), r.week_start.clone()), r))
        .collect();

    let week_idx_map: HashMap<&str, usize> = model
        .weeks
        .iter()
        .enumerate()
        .map(|(i, w)| (w.as_str(), i))
        .collect();

    for row in psi_rows {
        if row.shortage_qty <= 1e-4 && row.safety_stock_deficit <= 1e-4 {
            continue;
        }

        let is_shortage = row.shortage_qty > 1e-4;
        let issue_type = if is_shortage {
            "shortage".to_string()
        } else {
            "safety_stock_deficit".to_string()
        };
        let severity = if is_shortage {
            "critical".to_string()
        } else {
            "warning".to_string()
        };

        let t = *week_idx_map.get(row.week_start.as_str()).unwrap_or(&0);
        let deficit_amount = if is_shortage {
            row.shortage_qty
        } else {
            row.safety_stock_deficit
        };

        let mut factors: Vec<CausalFactor> = Vec::new();
        let mut actions: Vec<WhatIfAction> = Vec::new();
        let mut step = 1usize;

        // Step 1: Local symptom at (location, sku, week_start)
        let total_dem = row.sales_demand_qty + row.bom_demand_qty + row.transport_out_qty;
        let total_sup = row.opening_inv
            + (row.production_qty as f64)
            + (row.transport_in_qty as f64)
            + (row.procurement_in_qty as f64)
            + row.scheduled_receipt_qty;
        factors.push(CausalFactor {
            step,
            category: "local_balance".to_string(),
            location: row.location.clone(),
            target_id: row.sku.clone(),
            week_start: row.week_start.clone(),
            summary: if is_shortage {
                format!(
                    "{} の {}（{}週）にて総需要 {:.0} に対し総供給 {:.0}（期首在庫 {:.0} + 入庫 {:.0}）となり、{:.0} 個の欠品が発生。",
                    row.location,
                    row.sku,
                    row.week_start,
                    total_dem,
                    total_sup,
                    row.opening_inv,
                    total_sup - row.opening_inv,
                    row.shortage_qty
                )
            } else {
                format!(
                    "{} の {}（{}週）にて期末在庫 {:.0} が安全在庫目標 {:.0} を {:.0} 個下回っています。",
                    row.location,
                    row.sku,
                    row.week_start,
                    row.ending_inv,
                    row.safety_stock_target,
                    row.safety_stock_deficit
                )
            },
            metric_label: if is_shortage {
                "欠品量".to_string()
            } else {
                "安全在庫不足量".to_string()
            },
            metric_value: deficit_amount,
            limit_value: Some(total_dem),
        });
        step += 1;

        let mut primary_cause = "複合的な供給制約".to_string();

        // Check Frozen Fence on this node
        if let Some(f_row) = model.facts_lookup.get(&(
            row.location.clone(),
            row.sku.clone(),
            row.week_start.clone(),
        )) {
            let p_fence = f_row.production_fence_weeks.unwrap_or(0);
            let r_fence = f_row.replenishment_fence_weeks.unwrap_or(0);
            if (row.can_produce && (t as i64) < p_fence)
                || ((row.can_replenish || row.can_procure) && (t as i64) < r_fence)
            {
                primary_cause = "確定期間フェンス（Frozen Fence）による計画固定".to_string();
                factors.push(CausalFactor {
                    step,
                    category: "frozen_fence".to_string(),
                    location: row.location.clone(),
                    target_id: row.sku.clone(),
                    week_start: row.week_start.clone(),
                    summary: format!(
                        "第 {} 週（{}）は確定期間フェンス内のため、生産・補充数量の上積み変更がロックされています。",
                        t + 1,
                        row.week_start
                    ),
                    metric_label: "フェンス週数".to_string(),
                    metric_value: p_fence.max(r_fence) as f64,
                    limit_value: None,
                });
                step += 1;
            }
        }

        // Case A: Node is replenished via transport routes (e.g. DC receiving from Plant)
        let incoming_routes: Vec<_> = req
            .transport_routes
            .iter()
            .filter(|r| r.to_location == row.location && r.sku == row.sku)
            .collect();

        for route in &incoming_routes {
            let trans_lt = route.transport_lt.unwrap_or(0).max(0) as usize;
            if t < trans_lt {
                primary_cause = format!(
                    "輸送リードタイム制約（{} ➔ {}: LT {}週）および期首在庫不足",
                    route.from_location, route.to_location, trans_lt
                );
                factors.push(CausalFactor {
                    step,
                    category: "lead_time_trap".to_string(),
                    location: route.from_location.clone(),
                    target_id: route.sku.clone(),
                    week_start: row.week_start.clone(),
                    summary: format!(
                        "{} から {} への輸送リードタイムが {} 週あるため、{} 週の需要には計画開始前（t < 0）の出荷が必要であり、新規輸送が間に合いません（期首在庫 {:.0} 個のみで対応）。",
                        route.from_location,
                        route.to_location,
                        trans_lt,
                        row.week_start,
                        row.opening_inv
                    ),
                    metric_label: "輸送LT (週)".to_string(),
                    metric_value: trans_lt as f64,
                    limit_value: Some(t as f64),
                });
                step += 1;

                let current_init = model
                    .lookups
                    .init_inv_map
                    .get(&(row.location.clone(), row.sku.clone()))
                    .copied()
                    .unwrap_or(0.0);
                actions.push(WhatIfAction {
                    action_type: "increase_init_inventory".to_string(),
                    label: format!(
                        "{} の {} 期首在庫を +{:.0} 個増やす ({:.0} ➔ {:.0})",
                        row.location,
                        row.sku,
                        deficit_amount.ceil(),
                        current_init,
                        current_init + deficit_amount.ceil()
                    ),
                    description: "輸送リードタイム期間内の期近需要をカバーするため、下流拠点の期首在庫（または確定入荷予定）を増強します。".to_string(),
                    target_location: row.location.clone(),
                    target_id: row.sku.clone(),
                    week_start: None,
                    recommended_delta: deficit_amount.ceil(),
                    new_value: current_init + deficit_amount.ceil(),
                });
            } else {
                let ship_t = t - trans_lt;
                let ship_w = &model.weeks[ship_t];
                factors.push(CausalFactor {
                    step,
                    category: "upstream_transport".to_string(),
                    location: route.from_location.clone(),
                    target_id: route.sku.clone(),
                    week_start: ship_w.clone(),
                    summary: format!(
                        "{}（{}週）へ到着するには、上流拠点 {} から {} 週（輸送LT {}週前）に出荷が必要です。",
                        row.location, row.week_start, route.from_location, ship_w, trans_lt
                    ),
                    metric_label: "上流出荷週の輸送入荷量".to_string(),
                    metric_value: row.transport_in_qty as f64,
                    limit_value: None,
                });
                step += 1;

                // Inspect upstream plant in week `ship_w` (and earlier weeks)
                if let Some(up_psi) =
                    psi_map.get(&(route.from_location.clone(), route.sku.clone(), ship_w.clone()))
                {
                    if let Some(ref line_id) = up_psi.line_id {
                        if let Some(lu) = line_util_map.get(&(
                            route.from_location.clone(),
                            line_id.clone(),
                            ship_w.clone(),
                        )) {
                            if lu.utilization_pct >= 90.0 || lu.is_bottleneck {
                                primary_cause = format!(
                                    "上流工場 {} の製造ライン {} 能力ボトルネック（{}週 稼働率 {:.1}%）",
                                    route.from_location, line_id, ship_w, lu.utilization_pct
                                );
                                factors.push(CausalFactor {
                                    step,
                                    category: "line_capacity_bottleneck".to_string(),
                                    location: route.from_location.clone(),
                                    target_id: line_id.clone(),
                                    week_start: ship_w.clone(),
                                    summary: format!(
                                        "上流 {} の製造ライン {}（{}週）の稼働率が {:.1}%（使用 {:.1}h / 上限 {:.1}h、生産 {} 個 / 最大 {:.0} 個）に達し、これ以上の増産が制限されています（Shadow Price: {:.2}）。",
                                        route.from_location,
                                        line_id,
                                        ship_w,
                                        lu.utilization_pct,
                                        lu.used_hours,
                                        lu.available_hours,
                                        lu.used_quantity,
                                        lu.max_capacity_quantity,
                                        lu.shadow_price
                                    ),
                                    metric_label: "ライン稼働率 (%)".to_string(),
                                    metric_value: lu.utilization_pct,
                                    limit_value: Some(100.0),
                                });
                                step += 1;

                                // Calculate how much extra capacity is needed
                                let sk = (route.from_location.clone(), route.sku.clone());
                                let extra_qty = deficit_amount.ceil();
                                if lu.capacity_mode == "hours" {
                                    let rph = model
                                        .lookups
                                        .sku_production_rate
                                        .get(&sk)
                                        .map(|r| (r.rate_per_hour * r.yield_rate).max(1e-9))
                                        .unwrap_or(1.0);
                                    let extra_hours = (extra_qty / rph).ceil();
                                    actions.push(WhatIfAction {
                                        action_type: "increase_line_hours".to_string(),
                                        label: format!(
                                            "{} / {} の週間稼働可能時間を +{:.0}h 増強 ({:.0}h ➔ {:.0}h)",
                                            route.from_location,
                                            line_id,
                                            extra_hours,
                                            lu.available_hours,
                                            lu.available_hours + extra_hours
                                        ),
                                        description: format!(
                                            "{}週の不足分 {:.0} 個を追加生産できるよう、製造ライン {} の稼働時間上限（残業・休日稼働枠）を引き上げます。",
                                            ship_w, extra_qty, line_id
                                        ),
                                        target_location: route.from_location.clone(),
                                        target_id: line_id.clone(),
                                        week_start: Some(ship_w.clone()),
                                        recommended_delta: extra_hours,
                                        new_value: lu.available_hours + extra_hours,
                                    });
                                } else {
                                    actions.push(WhatIfAction {
                                        action_type: "increase_line_capacity".to_string(),
                                        label: format!(
                                            "{} / {} の週間能力上限を +{:.0} 個増強 ({:.0} ➔ {:.0})",
                                            route.from_location,
                                            line_id,
                                            extra_qty,
                                            lu.max_capacity_quantity,
                                            lu.max_capacity_quantity + extra_qty
                                        ),
                                        description: format!(
                                            "{}週の不足分 {:.0} 個を追加生産できるよう、ライン能力上限を引き上げます。",
                                            ship_w, extra_qty
                                        ),
                                        target_location: route.from_location.clone(),
                                        target_id: line_id.clone(),
                                        week_start: Some(ship_w.clone()),
                                        recommended_delta: extra_qty,
                                        new_value: lu.max_capacity_quantity + extra_qty,
                                    });
                                }
                            }
                        }
                    }

                    // Also check BOM child SKUs at the upstream plant
                    let bom_children: Vec<_> = req
                        .bom
                        .iter()
                        .filter(|b| {
                            b.target_location == route.from_location && b.parent_sku == route.sku
                        })
                        .collect();
                    for b in bom_children {
                        if let Some(child_psi) = psi_map.get(&(
                            route.from_location.clone(),
                            b.child_sku.clone(),
                            ship_w.clone(),
                        )) {
                            let child_cap = model
                                .lookups
                                .procurement_capacity_map
                                .get(&(route.from_location.clone(), b.child_sku.clone()))
                                .copied();
                            let child_lt = model
                                .lookups
                                .procurement_lt_map
                                .get(&(route.from_location.clone(), b.child_sku.clone()))
                                .copied()
                                .unwrap_or(0) as usize;

                            if ship_t < child_lt && child_psi.ending_inv <= 1e-4 {
                                primary_cause = format!(
                                    "原材料 {} の調達リードタイム制約（調達LT {}週）による親品目 {} 生産制限",
                                    b.child_sku, child_lt, route.sku
                                );
                                factors.push(CausalFactor {
                                    step,
                                    category: "bom_material_shortage".to_string(),
                                    location: route.from_location.clone(),
                                    target_id: b.child_sku.clone(),
                                    week_start: ship_w.clone(),
                                    summary: format!(
                                        "子品目 {}（員数 {:.1}）の調達LTが {} 週あるため {} 週の増産に必要な原材料が不足（期末在庫 {:.0}）。",
                                        b.child_sku, b.quantity, child_lt, ship_w, child_psi.ending_inv
                                    ),
                                    metric_label: "原材料期末在庫".to_string(),
                                    metric_value: child_psi.ending_inv,
                                    limit_value: Some(deficit_amount * b.quantity),
                                });
                                step += 1;
                            } else if let Some(cap) = child_cap {
                                if (child_psi.order_release_qty as f64) >= cap - 1e-4 {
                                    primary_cause = format!(
                                        "原材料 {} の週間調達能力上限（{:.0} 個/週）到達による生産制限",
                                        b.child_sku, cap
                                    );
                                    factors.push(CausalFactor {
                                        step,
                                        category: "procurement_capacity_bottleneck".to_string(),
                                        location: route.from_location.clone(),
                                        target_id: b.child_sku.clone(),
                                        week_start: ship_w.clone(),
                                        summary: format!(
                                            "子品目 {}（員数 {:.1}）の発注量が週間調達上限 {:.0} に到達しており、親品目 {} をこれ以上生産できません。",
                                            b.child_sku, b.quantity, cap, route.sku
                                        ),
                                        metric_label: "週間調達量".to_string(),
                                        metric_value: child_psi.order_release_qty as f64,
                                        limit_value: Some(cap),
                                    });
                                    step += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Case B: Node produces directly (e.g. Plant SKU has direct demand or shortage)
        if row.can_produce {
            if let Some(ref line_id) = row.line_id {
                if let Some(lu) = line_util_map.get(&(
                    row.location.clone(),
                    line_id.clone(),
                    row.week_start.clone(),
                )) {
                    if lu.utilization_pct >= 90.0 || lu.is_bottleneck {
                        primary_cause = format!(
                            "工場 {} の製造ライン {} 能力上限到達（{}週 稼働率 {:.1}%）",
                            row.location, line_id, row.week_start, lu.utilization_pct
                        );
                        factors.push(CausalFactor {
                            step,
                            category: "line_capacity_bottleneck".to_string(),
                            location: row.location.clone(),
                            target_id: line_id.clone(),
                            week_start: row.week_start.clone(),
                            summary: format!(
                                "製造ライン {} の稼働率が {:.1}%（上限到達）のため、不足分 {:.0} 個を追加生産できていません（Shadow Price: {:.2}）。",
                                line_id, lu.utilization_pct, deficit_amount, lu.shadow_price
                            ),
                            metric_label: "ライン稼働率 (%)".to_string(),
                            metric_value: lu.utilization_pct,
                            limit_value: Some(100.0),
                        });
                        step += 1;

                        let extra_qty = deficit_amount.ceil();
                        let sk = (row.location.clone(), row.sku.clone());
                        if lu.capacity_mode == "hours" {
                            let rph = model
                                .lookups
                                .sku_production_rate
                                .get(&sk)
                                .map(|r| (r.rate_per_hour * r.yield_rate).max(1e-9))
                                .unwrap_or(1.0);
                            let extra_hours = (extra_qty / rph).ceil();
                            actions.push(WhatIfAction {
                                action_type: "increase_line_hours".to_string(),
                                label: format!(
                                    "{} / {} の週間稼働可能時間を +{:.0}h 増強 ({:.0}h ➔ {:.0}h)",
                                    row.location,
                                    line_id,
                                    extra_hours,
                                    lu.available_hours,
                                    lu.available_hours + extra_hours
                                ),
                                description: "ラインの週間稼働可能時間を引き上げて生産能力ボトルネックを解消します。".to_string(),
                                target_location: row.location.clone(),
                                target_id: line_id.clone(),
                                week_start: Some(row.week_start.clone()),
                                recommended_delta: extra_hours,
                                new_value: lu.available_hours + extra_hours,
                            });
                        } else {
                            actions.push(WhatIfAction {
                                action_type: "increase_line_capacity".to_string(),
                                label: format!(
                                    "{} / {} の週間能力上限を +{:.0} 個増強 ({:.0} ➔ {:.0})",
                                    row.location,
                                    line_id,
                                    extra_qty,
                                    lu.max_capacity_quantity,
                                    lu.max_capacity_quantity + extra_qty
                                ),
                                description: "ラインの週間最大生産数量を引き上げてボトルネックを解消します。".to_string(),
                                target_location: row.location.clone(),
                                target_id: line_id.clone(),
                                week_start: Some(row.week_start.clone()),
                                recommended_delta: extra_qty,
                                new_value: lu.max_capacity_quantity + extra_qty,
                            });
                        }
                    }
                }
            }
        }

        // Fallback What-If action if none was added yet
        if actions.is_empty() {
            let current_init = model
                .lookups
                .init_inv_map
                .get(&(row.location.clone(), row.sku.clone()))
                .copied()
                .unwrap_or(0.0);
            actions.push(WhatIfAction {
                action_type: "increase_init_inventory".to_string(),
                label: format!(
                    "{} の {} 期首在庫を +{:.0} 個増強 ({:.0} ➔ {:.0})",
                    row.location,
                    row.sku,
                    deficit_amount.ceil(),
                    current_init,
                    current_init + deficit_amount.ceil()
                ),
                description: "不足分を期首在庫の積み増しでカバーするシミュレーションを実行します。"
                    .to_string(),
                target_location: row.location.clone(),
                target_id: row.sku.clone(),
                week_start: None,
                recommended_delta: deficit_amount.ceil(),
                new_value: current_init + deficit_amount.ceil(),
            });
        }

        let narrative = factors
            .iter()
            .map(|f| format!("[Step {}] {}", f.step, f.summary))
            .collect::<Vec<_>>()
            .join(" ➔ ");

        diagnostics.push(RootCauseDiagnostic {
            issue_type,
            severity,
            location: row.location.clone(),
            sku: row.sku.clone(),
            week_start: row.week_start.clone(),
            shortage_qty: row.shortage_qty,
            ss_deficit_qty: row.safety_stock_deficit,
            primary_cause,
            narrative,
            factors,
            actions,
        });
    }

    diagnostics
}
