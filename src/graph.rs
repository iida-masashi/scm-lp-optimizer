use crate::schema::{OptimizerError, ScheduledReceiptRow};
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

pub const TERMINAL_DEMAND_LOOKBACK_WEEKS: usize = 4;

/// Gap in days between each adjacent pair of bucket starts (`solver_lp_model._axis_bucket_days`).
pub fn axis_bucket_days(weeks: &[String]) -> Result<Vec<i64>, OptimizerError> {
    let mut dates = Vec::with_capacity(weeks.len());
    for w in weeks {
        let d = NaiveDate::parse_from_str(w, "%Y-%m-%d").map_err(|e| {
            OptimizerError::InvalidInput(format!("Invalid date string '{w}' in week_start: {e}"))
        })?;
        dates.push(d);
    }
    let mut gaps = Vec::with_capacity(dates.len().saturating_sub(1));
    for i in 0..dates.len().saturating_sub(1) {
        gaps.push((dates[i + 1] - dates[i]).num_days());
    }
    Ok(gaps)
}

/// Reject a non-uniform time axis before formulating the LP (`solver_lp_model._assert_uniform_weekly_axis`).
pub fn assert_uniform_weekly_axis(weeks: &[String]) -> Result<(), OptimizerError> {
    if weeks.len() < 2 {
        return Ok(());
    }
    let gaps = axis_bucket_days(weeks)?;
    let mut unique_gaps: Vec<i64> = gaps.into_iter().collect::<HashSet<_>>().into_iter().collect();
    unique_gaps.sort_unstable();
    if unique_gaps == vec![7] {
        return Ok(());
    }
    let reason = if unique_gaps.iter().all(|&d| d % 7 == 0) {
        format!(
            "facts の週次軸に欠落があります（バケット間隔 {:?} 日）。\
             週が丸ごと欠けているため、LP の t ± lt が実際のリードタイムとは\
             違う期間を指します。該当バージョンの facts を補完してから\
             再実行してください。",
            unique_gaps
        )
    } else {
        format!(
            "バケット長が週次でない軸が検出されました（バケット間隔 {:?} 日）。\
             LP は週次軸のみ対応しています。",
            unique_gaps
        )
    };
    Err(OptimizerError::SolverError {
        message: format!("LP はバケット長が一様な週次軸のみ対応しています。{reason}"),
        status: 0,
        solver_message: format!(
            "non-uniform planning axis: bucket gaps {:?} days over {} buckets",
            unique_gaps,
            weeks.len()
        ),
    })
}

/// Topological order where suppliers come AFTER their consumers (`solver_base._supplier_to_consumer_order`).
pub fn supplier_to_consumer_order(
    n: usize,
    bom_consumers: &HashMap<usize, Vec<(usize, f64, i64)>>,
    transport_consumers: &HashMap<usize, Vec<(usize, i64, f64)>>,
) -> Vec<usize> {
    let mut consumers_of: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (&src_p, edges) in transport_consumers {
        for &(dst_p, _, _) in edges {
            consumers_of.entry(src_p).or_default().insert(dst_p);
        }
    }
    for (&src_p, edges) in bom_consumers {
        for &(parent_p, _, _) in edges {
            consumers_of.entry(src_p).or_default().insert(parent_p);
        }
    }

    let mut remaining: HashMap<usize, HashSet<usize>> = (0..n)
        .map(|p| (p, consumers_of.get(&p).cloned().unwrap_or_default()))
        .collect();
    let mut ready: Vec<usize> = (0..n)
        .filter(|p| remaining.get(p).map_or(true, |s| s.is_empty()))
        .collect();
    let mut order = Vec::with_capacity(n);
    let mut seen = HashSet::with_capacity(n);

    while let Some(node) = ready.pop() {
        if seen.contains(&node) {
            continue;
        }
        seen.insert(node);
        order.push(node);
        for p in 0..n {
            if let Some(rem) = remaining.get_mut(&p) {
                if rem.remove(&node) && rem.is_empty() && !seen.contains(&p) {
                    ready.push(p);
                }
            }
        }
    }
    order
}

/// Enrich `demand_for_target` (shape N×T) with downstream-node pull, shifted by LT (`solver_lp_model._propagate_downstream_demand`).
pub fn propagate_downstream_demand(
    demand_for_target: &mut [Vec<f64>],
    bom_consumers: &HashMap<usize, Vec<(usize, f64, i64)>>,
    transport_consumers: &HashMap<usize, Vec<(usize, i64, f64)>>,
    n: usize,
    t_len: usize,
) {
    let mut out_edges: HashMap<usize, Vec<(usize, i64, f64)>> = HashMap::new();
    for (&src_p, consumers) in transport_consumers {
        for &(dst_p, trans_lt, quota) in consumers {
            out_edges
                .entry(src_p)
                .or_default()
                .push((dst_p, trans_lt, quota));
        }
    }
    for (&src_p, consumers) in bom_consumers {
        for &(parent_p, bom_qty, prod_lt) in consumers {
            out_edges
                .entry(src_p)
                .or_default()
                .push((parent_p, prod_lt, bom_qty));
        }
    }

    let order = supplier_to_consumer_order(n, bom_consumers, transport_consumers);
    for src_p in order {
        if let Some(edges) = out_edges.get(&src_p) {
            for &(dst_p, lt_raw, mult) in edges {
                let lt = lt_raw.max(0) as usize;
                if dst_p == src_p {
                    continue;
                }
                let dst_row = demand_for_target[dst_p].clone();
                if lt == 0 {
                    for t in 0..t_len {
                        demand_for_target[src_p][t] += mult * dst_row[t];
                    }
                } else if lt < t_len {
                    for t in 0..(t_len - lt) {
                        demand_for_target[src_p][t] += mult * dst_row[t + lt];
                    }
                }
            }
        }
    }
}

/// Compute per-week safety-stock target = sum of next `ss_weeks` of demand (`planning_helpers.compute_safety_stock_target`).
pub fn compute_safety_stock_target(
    demand_series: &[f64],
    ss_weeks: f64,
    lookback_weeks: Option<usize>,
) -> Vec<f64> {
    let t_len = demand_series.len();
    if ss_weeks <= 0.0 || t_len == 0 {
        return vec![0.0; t_len];
    }
    let lb_cfg = lookback_weeks.unwrap_or(TERMINAL_DEMAND_LOOKBACK_WEEKS);
    let lookback = (if lb_cfg == 0 {
        TERMINAL_DEMAND_LOOKBACK_WEEKS
    } else {
        lb_cfg
    })
    .min(t_len);

    let beyond = if lookback > 0 {
        let sum_tail: f64 = (1..=lookback).map(|k| demand_series[t_len - k]).sum();
        sum_tail / (lookback as f64)
    } else {
        0.0
    };

    let full = ss_weeks.floor() as usize;
    let frac = ss_weeks - (full as f64);
    let mut out = vec![0.0; t_len];
    for t in 0..t_len {
        let mut total = 0.0;
        for j in 1..=full {
            total += if t + j < t_len {
                demand_series[t + j]
            } else {
                beyond
            };
        }
        if frac > 0.0 {
            let next_val = if t + full + 1 < t_len {
                demand_series[t + full + 1]
            } else {
                beyond
            };
            total += next_val * frac;
        }
        out[t] = total;
    }
    out
}

/// Bucket scheduled receipts onto the planning week axis (`planning_helpers.bucket_receipts_by_week`).
pub fn bucket_receipts_by_week(
    receipt_rows: &[ScheduledReceiptRow],
    weeks: &[String],
) -> HashMap<(String, String), HashMap<String, f64>> {
    let mut out: HashMap<(String, String), HashMap<String, f64>> = HashMap::new();
    if receipt_rows.is_empty() || weeks.is_empty() {
        return out;
    }
    let week_dates: Vec<NaiveDate> = weeks
        .iter()
        .filter_map(|w| NaiveDate::parse_from_str(w, "%Y-%m-%d").ok())
        .collect();
    if week_dates.len() != weeks.len() {
        return out;
    }
    let last_week_end = *week_dates.last().unwrap() + chrono::Duration::days(6);

    for r in receipt_rows {
        if r.qty <= 0.0 {
            continue;
        }
        let Ok(due) = NaiveDate::parse_from_str(&r.due_week, "%Y-%m-%d") else {
            continue;
        };
        if due > last_week_end {
            continue;
        }
        // bisect_right(week_dates, due) - 1, floored at 0
        let pos = week_dates.partition_point(|&wd| wd <= due);
        let t = pos.saturating_sub(1);
        let wk = weeks[t].clone();
        let bucket = out
            .entry((r.location.clone(), r.sku.clone()))
            .or_default();
        *bucket.entry(wk).or_insert(0.0) += r.qty;
    }
    out
}
