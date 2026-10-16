pub mod analytics;
pub mod graph;
pub mod lookups;
pub mod lp_model;
pub mod postprocess;
pub mod qa;
pub mod schema;
pub mod solver_clarabel;

use std::time::Instant;

use crate::analytics::{
    compute_line_utilizations, compute_root_causes, compute_sku_psi_details,
    compute_transport_flows,
};
use crate::lp_model::{build_lp_model, formulate_lp};
use crate::postprocess::extract_postprocess_output;
use crate::schema::{OptimizeRequestPayload, OptimizeResponsePayload, OptimizerError};
use crate::solver_clarabel::solve_lp_clarabel;

/// Main entry point for the multi-period capacitated LP supply chain optimizer.
pub fn run_optimization(
    req: &OptimizeRequestPayload,
) -> Result<OptimizeResponsePayload, OptimizerError> {
    let t0 = Instant::now();

    let Some(model) = build_lp_model(req)? else {
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
        });
    };

    let formulated = formulate_lp(&model, &req.config);
    let solve_res = solve_lp_clarabel(&formulated)?;
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
    })
}

