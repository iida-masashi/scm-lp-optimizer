pub mod analytics;
pub mod graph;
pub mod lookups;
pub mod lp_model;
pub mod postprocess;
pub mod qa;
pub mod schema;
pub mod milp;
pub mod solver_clarabel;

use std::time::Instant;

use crate::analytics::{
    compute_line_utilizations, compute_root_causes, compute_sku_psi_details,
    compute_transport_flows,
};
use crate::lp_model::{
    apply_relax_firm, build_lp_model, formulate_lp, var_idx, FormulatedLp, LpModel, SLOT_E, SLOT_P, SLOT_R,
};
use crate::postprocess::extract_postprocess_output;
use crate::schema::{
    AltLineUsageRow, ModelWarning, OptimizeRequestPayload, OptimizeResponsePayload, OptimizerError,
};
use crate::solver_clarabel::solve_lp_clarabel;

/// Main entry point for the multi-period capacitated LP supply chain optimizer.
pub fn run_optimization(
    req: &OptimizeRequestPayload,
) -> Result<OptimizeResponsePayload, OptimizerError> {
    let t0 = Instant::now();

    let Some(mut model) = build_lp_model(req)? else {
        return Ok(OptimizeResponsePayload {
            status: "optimal".to_string(),
            elapsed_ms: t0.elapsed().as_millis() as u64,
            facts: req.facts.clone(),
            edits: Vec::new(),
            bottlenecks: Vec::new(),
            line_utilizations: Vec::new(),
            transport_flows: Vec::new(),
            sku_psi_details: Vec::new(),
            root_causes: Vec::new(),
            warnings: Vec::new(),
            alt_line_usage: Vec::new(),
        });
    };

    let use_milp = req.config.solver.eq_ignore_ascii_case("milp");
    let lp0 = formulate_lp(&model, &req.config);
    let mut milp_form = if use_milp { Some(crate::milp::formulate_milp(&model, lp0.clone())) } else { None };
    let mut formulated = match &milp_form {
        Some(m) => m.lp.clone(),
        None => lp0,
    };
    let run = |f: &FormulatedLp, m: &Option<crate::milp::MilpFormulation>| match m {
        Some(m) => crate::milp::solve_milp(&crate::milp::MilpFormulation {
            lp: f.clone(),
            integer_cols: m.integer_cols.clone(),
        }),
        None => solve_lp_clarabel(f),
    };
    let solve_res = match run(&formulated, &milp_form) {
        Ok(r) => r,
        // Infeasible with firmed cells: relax them to `[0, qty]` with a large reward and retry.
        Err(OptimizerError::SolverError { status: 2, .. }) if model.has_firm_cells => {
            model.relax_firm = true;
            apply_relax_firm(&model, &req.config, &mut formulated);
            if let Some(m) = milp_form.as_mut() {
                m.lp = formulated.clone();
            }
            run(&formulated, &milp_form)?
        }
        Err(e) => return Err(e),
    };

    let mut warnings = model.warnings.clone();
    if model.relax_firm {
        warnings.extend(firm_shortfall_warnings(&model, &solve_res.x));
    }
    let alt_line_usage = alt_line_usage_rows(&model, &formulated, &solve_res.x);
    if !model.dynamic_transport_edges.is_empty() {
        warnings.push(ModelWarning {
            code: "lane_split_not_persisted".to_string(),
            location: None,
            sku: None,
            week: None,
            message: "複数経路の出荷内訳は計画列に保存されません。内訳は transport_flows を参照してください。".to_string(),
        });
    }
    if !alt_line_usage.is_empty() {
        warnings.push(ModelWarning {
            code: "alt_line_split_not_persisted".to_string(),
            location: None,
            sku: None,
            week: None,
            message: "代替ラインへの割当は計画列に保存されません。内訳は alt_line_usage を参照してください。"
                .to_string(),
        });
    }
    let post = extract_postprocess_output(&solve_res.x, &model, req);

    let line_utilizations =
        compute_line_utilizations(&post.prod_raw, &model, req, &solve_res.bottlenecks);
    let transport_flows = compute_transport_flows(&post.ship_out, &model, req);
    let sku_psi_details = compute_sku_psi_details(
        &post.prod_raw,
        &post.replen_out,
        &post.release_out,
        &post.ship_out,
        &solve_res.x,
        &model,
        req,
    );
    let root_causes = compute_root_causes(&sku_psi_details, &line_utilizations, &model, req);

    Ok(OptimizeResponsePayload {
        status: "optimal".to_string(),
        elapsed_ms: t0.elapsed().as_millis() as u64,
        facts: post.updated_facts,
        edits: post.edits,
        bottlenecks: solve_res.bottlenecks,
        line_utilizations,
        transport_flows,
        sku_psi_details,
        root_causes,
        warnings,
        alt_line_usage,
    })
}

/// `_check_firm_shortfall`: protected cells that the relaxed solution could not honour.
fn firm_shortfall_warnings(model: &LpModel, x: &[f64]) -> Vec<ModelWarning> {
    let t_len = model.t_len;
    let mut out = Vec::new();
    let mut push = |p: usize, t: usize, kind: &str, firm: f64, got: f64| {
        if firm - got > 1e-6 {
            out.push(ModelWarning {
                code: "firm_qty_not_feasible".to_string(),
                location: Some(model.all_pairs[p].location.clone()),
                sku: Some(model.all_pairs[p].sku.clone()),
                week: Some(model.weeks[t].chars().take(10).collect()),
                message: format!(
                    "確定数量を満たす部品・供給元・能力が足りません。({kind}: 確定 {firm:.4} / 実行可能 {got:.4})"
                ),
            });
        }
    };
    let row_of = |p: usize, t: usize| {
        model.facts_lookup.get(&(
            model.all_pairs[p].location.clone(),
            model.all_pairs[p].sku.clone(),
            model.weeks[t].clone(),
        ))
    };
    let mut cells: Vec<_> = model.protected_p.iter().copied().collect();
    cells.sort();
    for (p, t) in cells {
        let firm = row_of(p, t).map(|r| r.production_qty as f64).unwrap_or(0.0);
        push(p, t, "production", firm, x[var_idx(p, t, SLOT_P, t_len)]);
    }
    let mut cells: Vec<_> = model.protected_r.iter().copied().collect();
    cells.sort();
    for (p, t) in cells {
        let fp = model.firm_purchase.get(&(p, t)).copied().unwrap_or(0.0);
        let firm = (row_of(p, t).map(|r| r.replenishment_qty as f64).unwrap_or(0.0) - fp).max(0.0);
        push(p, t, "replenishment", firm, x[var_idx(p, t, SLOT_R, t_len)]);
    }
    let mut cells: Vec<_> = model.protected_e.iter().copied().collect();
    cells.sort();
    for (p, te) in cells {
        let key = (model.all_pairs[p].location.clone(), model.all_pairs[p].sku.clone());
        let e_lt = model.lookups.procurement_lt_map.get(&key).copied().unwrap_or(0) as usize;
        let firm = model.firm_purchase.get(&(p, te + e_lt)).copied().unwrap_or(0.0);
        push(p, te, "procurement", firm, x[var_idx(p, te, SLOT_E, t_len)]);
    }
    out
}

fn alt_line_usage_rows(model: &LpModel, lp: &FormulatedLp, x: &[f64]) -> Vec<AltLineUsageRow> {
    let Some(base) = lp.pa_base else { return Vec::new() };
    let t_len = model.t_len;
    let mut out = Vec::new();
    for (e_idx, e) in model.alt_line_edges.iter().enumerate() {
        for t in 0..t_len {
            let q = x[base + e_idx * t_len + t];
            if q > 1e-6 {
                out.push(AltLineUsageRow {
                    location: e.location.clone(),
                    sku: e.sku.clone(),
                    alt_line: e.alt_line.clone(),
                    week_start: model.weeks[t].clone(),
                    qty: (q * 10000.0).round() / 10000.0,
                });
            }
        }
    }
    out
}

