pub mod graph;
pub mod lookups;
pub mod lp_model;
pub mod postprocess;
pub mod schema;
pub mod solver_clarabel;

use std::time::Instant;

use crate::lp_model::{build_lp_model, formulate_lp};
use crate::postprocess::extract_and_write_back;
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
        });
    };

    let formulated = formulate_lp(&model, &req.config);
    let solve_res = solve_lp_clarabel(&formulated)?;
    let (updated_facts, edits) = extract_and_write_back(&solve_res.x, &model, req);

    Ok(OptimizeResponsePayload {
        status: "optimal".to_string(),
        elapsed_ms: t0.elapsed().as_millis() as u64,
        facts: updated_facts,
        edits,
        bottlenecks: solve_res.bottlenecks,
    })
}
