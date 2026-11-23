//! MILP backend (openSCM `solver_highs._highs_add_lot_and_setup_constraints` + `_solve_highs_milp`).
//!
//! The continuous LP built by `formulate_lp` is extended with
//!   * `Z` integer lot-count variables (`P = lot * Z`) for production / shipping / procurement lots,
//!   * `Y` binary setup / minimum-batch variables (`P <= M*Y`, `P >= min*Y`), with `setup_cost`
//!     charged in the objective and `setup_time_hours` consuming line capacity,
//! and solved with HiGHS (`highs` crate, branch & cut). Duals do not exist for a MILP,
//! so bottlenecks are reported as binding rows with marginal value 1.0 (same as openSCM).

use std::collections::{HashMap, HashSet};

use crate::graph::supplier_to_consumer_order;
use crate::lp_model::{
    var_idx, FormulatedLp, LpModel, SparseRow, SLOT_E, SLOT_P, SLOT_R,
};
use crate::schema::{Bottleneck, OptimizerError};
use crate::solver_clarabel::ClarabelSolveResult;

/// LP plus the indices of its integer / binary columns.
#[derive(Debug, Clone)]
pub struct MilpFormulation {
    pub lp: FormulatedLp,
    pub integer_cols: Vec<usize>,
}

/// `_compute_milp_pair_ub`: valid horizon upper bound per pair, propagated upstream along BOM/transport.
pub fn compute_milp_pair_ub(model: &LpModel) -> Vec<f64> {
    let n = model.n;
    let mut ub: Vec<f64> = (0..n)
        .map(|p| {
            model.demand[p].iter().sum::<f64>() + model.target_inv[p].iter().sum::<f64>()
        })
        .collect();
    if let Some(ref ex) = model.projected_expiries {
        for p in 0..n {
            ub[p] += ex[p].iter().sum::<f64>();
        }
    }
    let has_dyn = !model.dynamic_transport_edges.is_empty();
    // consumers first, so every consumer's bound is final before its suppliers are processed
    let order = supplier_to_consumer_order(n, &model.bom_consumers, &model.transport_consumers);
    for src in order {
        let mut extra = 0.0;
        if let Some(edges) = model.bom_consumers.get(&src) {
            for &(parent, qty, _) in edges {
                extra += qty * ub[parent];
            }
        }
        if let Some(edges) = model.transport_consumers.get(&src) {
            for &(dst, _, mult) in edges {
                let m = if has_dyn { mult.max(1.0) } else { mult };
                extra += m * ub[dst];
            }
        }
        ub[src] += extra;
    }
    ub
}

struct Builder<'a> {
    lp: &'a mut FormulatedLp,
    integer_cols: Vec<usize>,
}

impl Builder<'_> {
    fn add_var(&mut self, lo: f64, hi: f64, cost: f64) -> usize {
        let j = self.lp.num_vars;
        self.lp.num_vars += 1;
        self.lp.bounds.push((lo, hi));
        self.lp.c.push(cost);
        self.integer_cols.push(j);
        j
    }
    fn eq(&mut self, terms: Vec<(usize, f64)>, rhs: f64) {
        self.lp.eq_rows.push(SparseRow { terms, rhs, bottleneck_desc: None });
    }
    fn le(&mut self, terms: Vec<(usize, f64)>, rhs: f64) {
        self.lp.ub_rows.push(SparseRow { terms, rhs, bottleneck_desc: None });
    }
}

/// Extend the continuous LP with lot (`Z`) and setup (`Y`) variables.
pub fn formulate_milp(model: &LpModel, mut lp: FormulatedLp) -> MilpFormulation {
    let t_len = model.t_len;
    let pair_ub = compute_milp_pair_ub(model);
    let has_alt: HashSet<usize> = model.alt_line_edges.iter().map(|e| e.p).collect();
    let lk = &model.lookups;
    let line_row_idx = lp.line_row_idx.clone();
    let line_mode = lp.line_mode.clone();
    let mut b = Builder { lp: &mut lp, integer_cols: Vec::new() };

    // Z upper bound (in lots) derived from the cell bound and the valid horizon bound.
    let z_ub = |hi: f64, horizon: f64, lot: f64| -> f64 {
        let cap = if hi.is_finite() { hi.min(horizon.max(lot)) } else { horizon.max(lot) };
        ((cap / lot).ceil() + 1.0).min(1.0e9)
    };

    for (p, pair) in model.all_pairs.iter().enumerate() {
        let key = (pair.location.clone(), pair.sku.clone());
        let horizon_ub = pair_ub[p].max(1.0);

        if pair.can_produce {
            let (p_min, p_lot) = lk.production_lot_map.get(&key).copied().unwrap_or((0, 1));
            let s_cost = lk.setup_cost_map.get(&key).copied().unwrap_or(0.0);
            let s_time = lk.setup_time_map.get(&key).copied().unwrap_or(0.0);
            let prim_line = lk.sku_to_line.get(&key);
            if p_min > 0 || p_lot > 1 || s_cost > 0.0 || s_time > 0.0 {
                for t in 0..t_len {
                    let p_vi = var_idx(p, t, SLOT_P, t_len);
                    let (lo, hi) = b.lp.bounds[p_vi];
                    if model.protected_p.contains(&(p, t)) || hi <= 0.0 || lo >= hi {
                        continue;
                    }
                    if p_lot > 1 {
                        let lot = p_lot as f64;
                        let zp = b.add_var(0.0, z_ub(hi, horizon_ub, lot), 0.0);
                        b.eq(vec![(p_vi, 1.0), (zp, -lot)], 0.0);
                    }
                    if p_min > 0 || s_cost > 0.0 || s_time > 0.0 {
                        let mut ubs = vec![horizon_ub];
                        if hi.is_finite() {
                            ubs.push(hi);
                        }
                        if !has_alt.contains(&p) {
                            if let Some(line) = prim_line {
                                if let Some(&(tb, hrs, max_cap)) =
                                    line_mode.get(&(pair.location.clone(), line.clone()))
                                {
                                    if tb {
                                        if let Some(ri) = lk.sku_production_rate.get(&key) {
                                            let h = model
                                                .calendar
                                                .hours_t
                                                .get(&(pair.location.clone(), line.clone(), t))
                                                .copied()
                                                .unwrap_or(hrs);
                                            ubs.push(h * ri.rate_per_hour * ri.yield_rate);
                                        }
                                    } else {
                                        ubs.push(
                                            model
                                                .calendar
                                                .line_cap_t
                                                .get(&(pair.location.clone(), line.clone(), t))
                                                .copied()
                                                .unwrap_or(max_cap),
                                        );
                                    }
                                }
                            } else if let Some(&lc) = lk.location_capacity.get(&pair.location) {
                                ubs.push(
                                    model
                                        .calendar
                                        .loc_cap_t
                                        .get(&(pair.location.clone(), t))
                                        .copied()
                                        .unwrap_or(lc),
                                );
                            }
                        }
                        let min_ub = ubs.iter().cloned().fold(f64::INFINITY, f64::min);
                        let big_m = min_ub.max(p_min as f64).max(p_lot as f64).max(1.0);

                        let yp = b.add_var(0.0, 1.0, if s_cost > 0.0 { s_cost } else { 0.0 });
                        b.le(vec![(p_vi, 1.0), (yp, -big_m)], 0.0);
                        if p_min > 0 {
                            b.le(vec![(p_vi, -1.0), (yp, p_min as f64)], 0.0);
                        }
                        if s_time > 0.0 {
                            if let Some(line) = prim_line {
                                let lkey = (pair.location.clone(), line.clone());
                                if let Some(&row) = line_row_idx.get(&(
                                    pair.location.clone(),
                                    line.clone(),
                                    t,
                                )) {
                                    let (tb, hrs, max_cap) =
                                        line_mode.get(&lkey).copied().unwrap_or((false, 0.0, 0.0));
                                    let coeff = if tb {
                                        s_time
                                    } else if hrs > 0.0 && max_cap > 0.0 {
                                        (max_cap / hrs) * s_time
                                    } else {
                                        0.0
                                    };
                                    if coeff > 0.0 {
                                        b.lp.ub_rows[row].terms.push((yp, coeff));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if pair.can_replenish {
            let (s_min, s_lot) = lk.shipping_lot_map.get(&key).copied().unwrap_or((0, 1));
            if s_min > 0 || s_lot > 1 {
                for t in 0..t_len {
                    let r_vi = var_idx(p, t, SLOT_R, t_len);
                    let (lo, hi) = b.lp.bounds[r_vi];
                    if model.protected_r.contains(&(p, t)) || hi <= 0.0 || lo >= hi {
                        continue;
                    }
                    if s_lot > 1 {
                        let lot = s_lot as f64;
                        let zr = b.add_var(0.0, z_ub(hi, horizon_ub, lot), 0.0);
                        b.eq(vec![(r_vi, 1.0), (zr, -lot)], 0.0);
                    }
                    if s_min > 0 {
                        let r_ub = if hi.is_finite() { hi } else { horizon_ub };
                        let big_m = r_ub.min(horizon_ub).max(s_min as f64).max(s_lot as f64).max(1.0);
                        let yr = b.add_var(0.0, 1.0, 0.0);
                        b.le(vec![(r_vi, 1.0), (yr, -big_m)], 0.0);
                        b.le(vec![(r_vi, -1.0), (yr, s_min as f64)], 0.0);
                    }
                }
            }
        }

        if pair.can_procure {
            let (pr_min, pr_lot) = lk.procurement_lot_map.get(&key).copied().unwrap_or((0, 1));
            if pr_min > 0 || pr_lot > 1 {
                for t in 0..t_len {
                    let e_vi = var_idx(p, t, SLOT_E, t_len);
                    let (lo, hi) = b.lp.bounds[e_vi];
                    if hi <= 0.0 || lo >= hi || model.protected_e.contains(&(p, t)) {
                        continue;
                    }
                    if pr_lot > 1 {
                        let lot = pr_lot as f64;
                        let ze = b.add_var(0.0, z_ub(hi, horizon_ub, lot), 0.0);
                        b.eq(vec![(e_vi, 1.0), (ze, -lot)], 0.0);
                    }
                    if pr_min > 0 {
                        let e_ub = if hi.is_finite() { hi } else { horizon_ub };
                        let big_m = e_ub.min(horizon_ub).max(pr_min as f64).max(pr_lot as f64).max(1.0);
                        let ye = b.add_var(0.0, 1.0, 0.0);
                        b.le(vec![(e_vi, 1.0), (ye, -big_m)], 0.0);
                        b.le(vec![(e_vi, -1.0), (ye, pr_min as f64)], 0.0);
                    }
                }
            }
        }
    }

    let integer_cols = b.integer_cols;
    MilpFormulation { lp, integer_cols }
}

fn merge_terms(terms: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let mut m: HashMap<usize, f64> = HashMap::new();
    for &(j, v) in terms {
        *m.entry(j).or_insert(0.0) += v;
    }
    let mut out: Vec<(usize, f64)> = m.into_iter().filter(|(_, v)| v.abs() > 1e-15).collect();
    out.sort_by_key(|e| e.0);
    out
}

/// Solve with HiGHS (branch & cut). `time_limit_secs` bounds the search; the incumbent is returned.
pub fn solve_milp(mf: &MilpFormulation, time_limit_secs: f64, rel_gap: f64) -> Result<ClarabelSolveResult, OptimizerError> {
    use highs::{HighsModelStatus, RowProblem, Sense};

    let lp = &mf.lp;
    let n = lp.num_vars;
    if n == 0 {
        return Ok(ClarabelSolveResult { x: Vec::new(), bottlenecks: Vec::new() });
    }
    let int_set: HashSet<usize> = mf.integer_cols.iter().copied().collect();
    let mut pb = RowProblem::default();
    let mut cols = Vec::with_capacity(n);
    for j in 0..n {
        let (lo, hi) = lp.bounds[j];
        let col = if int_set.contains(&j) {
            pb.add_integer_column(lp.c[j], lo..hi)
        } else {
            pb.add_column(lp.c[j], lo..hi)
        };
        cols.push(col);
    }
    for row in &lp.eq_rows {
        let t: Vec<_> = merge_terms(&row.terms).into_iter().map(|(j, v)| (cols[j], v)).collect();
        pb.add_row(row.rhs..=row.rhs, t);
    }
    for row in &lp.ub_rows {
        let t: Vec<_> = merge_terms(&row.terms).into_iter().map(|(j, v)| (cols[j], v)).collect();
        pb.add_row(..=row.rhs, t);
    }

    let mut model = pb.optimise(Sense::Minimise);
    model.set_option("output_flag", false);
    model.set_option("time_limit", time_limit_secs);
    model.set_option("mip_rel_gap", rel_gap);
    let solved = model.solve();
    let status = solved.status();
    match status {
        HighsModelStatus::Infeasible => {
            return Err(OptimizerError::SolverError {
                message: "制約条件を同時に満たす実行可能解が存在しません（ロット・Capacity / Fence / 初期在庫の矛盾を確認してください）。".to_string(),
                status: 2,
                solver_message: format!("HiGHS {status:?}"),
            })
        }
        HighsModelStatus::Unbounded | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(OptimizerError::SolverError {
                message: "目的関数が非有界（Unbounded）または実行不能です。コスト設定を確認してください。".to_string(),
                status: 3,
                solver_message: format!("HiGHS {status:?}"),
            })
        }
        _ => {}
    }
    let mut x: Vec<f64> = solved.get_solution().columns().to_vec();
    if x.len() != n {
        return Err(OptimizerError::SolverError {
            message: format!("MILP が解を返しませんでした（{status:?}）。時間制限内に実行可能解が見つからなかった可能性があります。"),
            status: 4,
            solver_message: format!("HiGHS {status:?}"),
        });
    }
    for &j in &mf.integer_cols {
        x[j] = x[j].round();
    }

    let mut bottlenecks = Vec::new();
    for row in &lp.ub_rows {
        if let Some(ref desc) = row.bottleneck_desc {
            if row.rhs <= 0.0 || row.rhs >= 1e15 {
                continue;
            }
            let act: f64 = row.terms.iter().map(|&(j, v)| v * x[j]).sum();
            if act >= row.rhs - 1e-4 {
                bottlenecks.push(Bottleneck { description: desc.clone(), marginal_value: 1.0 });
            }
        }
    }
    Ok(ClarabelSolveResult { x, bottlenecks })
}
