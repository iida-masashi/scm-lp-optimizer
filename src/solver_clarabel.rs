use clarabel::algebra::CscMatrix;
use clarabel::solver::{
    DefaultSettingsBuilder, DefaultSolver, IPSolver, SolverStatus, SupportedConeT,
};

use crate::lp_model::FormulatedLp;
use crate::schema::{Bottleneck, OptimizerError};

pub struct ClarabelSolveResult {
    pub x: Vec<f64>,
    pub bottlenecks: Vec<Bottleneck>,
}

/// Build a CSC matrix from (row, col, val) triplets.
/// Combines duplicate (row, col) entries by summing their values.
fn triplets_to_csc(
    nrows: usize,
    ncols: usize,
    mut triplets: Vec<(usize, usize, f64)>,
) -> CscMatrix<f64> {
    triplets.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut colptr = Vec::with_capacity(ncols + 1);
    let mut rowval = Vec::new();
    let mut nzval = Vec::new();

    colptr.push(0);
    let mut current_col = 0;

    for (r, c, v) in triplets {
        if v.abs() < 1e-15 {
            continue;
        }
        while current_col < c {
            colptr.push(rowval.len());
            current_col += 1;
        }
        if let Some(&last_r) = rowval.last() {
            if colptr.len() - 1 == c && rowval.len() > *colptr.last().unwrap() && last_r == r {
                let last_idx = nzval.len() - 1;
                nzval[last_idx] += v;
                continue;
            }
        }
        rowval.push(r);
        nzval.push(v);
    }
    while current_col < ncols {
        colptr.push(rowval.len());
        current_col += 1;
    }

    CscMatrix::new(nrows, ncols, colptr, rowval, nzval)
}

/// Solve the formulated LP using Clarabel's interior-point solver.
pub fn solve_lp_clarabel(formulation: &FormulatedLp) -> Result<ClarabelSolveResult, OptimizerError> {
    let n = formulation.num_vars;
    if n == 0 {
        return Ok(ClarabelSolveResult {
            x: Vec::new(),
            bottlenecks: Vec::new(),
        });
    }

    let mut triplets: Vec<(usize, usize, f64)> = Vec::new();
    let mut b: Vec<f64> = Vec::new();

    // 1. Equality constraints (ZeroConeT)
    let mut n_eq = 0usize;
    for row in &formulation.eq_rows {
        let row_idx = b.len();
        for &(col, val) in &row.terms {
            triplets.push((row_idx, col, val));
        }
        b.push(row.rhs);
        n_eq += 1;
    }

    // Fixed bounds: lb == ub
    for (j, &(lb, ub)) in formulation.bounds.iter().enumerate() {
        if (ub - lb).abs() < 1e-12 && lb.is_finite() {
            let row_idx = b.len();
            triplets.push((row_idx, j, 1.0));
            b.push(lb);
            n_eq += 1;
        }
    }

    // 2. Inequality constraints (NonnegativeConeT)
    let ub_start_row = b.len();
    let mut n_ineq = 0usize;
    for row in &formulation.ub_rows {
        let row_idx = b.len();
        for &(col, val) in &row.terms {
            triplets.push((row_idx, col, val));
        }
        b.push(row.rhs);
        n_ineq += 1;
    }

    // Variable lower bounds: x_j >= lb_j <=> -x_j + s = -lb_j, s >= 0
    for (j, &(lb, ub)) in formulation.bounds.iter().enumerate() {
        if (ub - lb).abs() >= 1e-12 && lb.is_finite() {
            let row_idx = b.len();
            triplets.push((row_idx, j, -1.0));
            b.push(-lb);
            n_ineq += 1;
        }
    }

    // Variable upper bounds: x_j <= ub_j <=> x_j + s = ub_j, s >= 0
    for (j, &(lb, ub)) in formulation.bounds.iter().enumerate() {
        if (ub - lb).abs() >= 1e-12 && ub.is_finite() {
            let row_idx = b.len();
            triplets.push((row_idx, j, 1.0));
            b.push(ub);
            n_ineq += 1;
        }
    }

    let m = b.len();
    let p_mat = CscMatrix::<f64>::zeros((n, n));
    let a_mat = triplets_to_csc(m, n, triplets);

    let mut cones = Vec::new();
    if n_eq > 0 {
        cones.push(SupportedConeT::ZeroConeT(n_eq));
    }
    if n_ineq > 0 {
        cones.push(SupportedConeT::NonnegativeConeT(n_ineq));
    }

    let settings = DefaultSettingsBuilder::default()
        .verbose(false)
        .max_step_fraction(0.99)
        .tol_gap_abs(1e-8)
        .tol_gap_rel(1e-8)
        .tol_feas(1e-8)
        .build()
        .map_err(|e| OptimizerError::SolverError {
            message: "Clarabel settings build failed".to_string(),
            status: -1,
            solver_message: format!("{:?}", e),
        })?;

    let mut solver = DefaultSolver::new(&p_mat, &formulation.c, &a_mat, &b, &cones, settings);
    solver.solve();

    let status = solver.solution.status;
    match status {
        SolverStatus::Solved | SolverStatus::AlmostSolved => {
            let x = solver.solution.x.clone();
            let z = &solver.solution.z;

            // Extract bottlenecks from dual variables of capacity/storage rows.
            // Clarabel dual z >= 0 corresponds to -marginals in SciPy HiGHS (d(Obj)/d(b_ub) = -z).
            let mut bottlenecks = Vec::new();
            for (idx, row) in formulation.ub_rows.iter().enumerate() {
                if let Some(ref desc) = row.bottleneck_desc {
                    let global_row = ub_start_row + idx;
                    if global_row < z.len() {
                        let marginal = -z[global_row];
                        if marginal.abs() > 1e-4 {
                            bottlenecks.push(Bottleneck {
                                description: desc.clone(),
                                marginal_value: (marginal * 10000.0).round() / 10000.0,
                            });
                        }
                    }
                }
            }

            Ok(ClarabelSolveResult { x, bottlenecks })
        }
        SolverStatus::PrimalInfeasible | SolverStatus::AlmostPrimalInfeasible => {
            Err(OptimizerError::SolverError {
                message: "制約条件を同時に満たす実行可能解が存在しません（Capacity / Fence / 初期在庫の矛盾を確認してください）。".to_string(),
                status: 2,
                solver_message: format!("{:?}", status),
            })
        }
        SolverStatus::DualInfeasible | SolverStatus::AlmostDualInfeasible => {
            Err(OptimizerError::SolverError {
                message: "目的関数が非有界（Unbounded）です。コスト設定を確認してください。".to_string(),
                status: 3,
                solver_message: format!("{:?}", status),
            })
        }
        other => Err(OptimizerError::SolverError {
            message: "LP ソルバーが最適解に収束しませんでした。".to_string(),
            status: 4,
            solver_message: format!("{:?}", other),
        }),
    }
}
