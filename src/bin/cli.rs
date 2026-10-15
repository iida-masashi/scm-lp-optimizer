use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process;

use scm_lp_optimizer::run_optimization;
use scm_lp_optimizer::schema::OptimizeRequestPayload;

fn print_usage(prog: &str) {
    eprintln!("Usage: {} [--input <input.json>] [--output <output.json>]", prog);
    eprintln!("  If --input is omitted or '-', reads JSON from stdin.");
    eprintln!("  If --output is omitted or '-', writes JSON to stdout.");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut input_path: Option<String> = None;
    let mut output_path: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--input" => {
                if i + 1 < args.len() {
                    input_path = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    print_usage(&args[0]);
                    process::exit(1);
                }
            }
            "-o" | "--output" => {
                if i + 1 < args.len() {
                    output_path = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    print_usage(&args[0]);
                    process::exit(1);
                }
            }
            "-h" | "--help" => {
                print_usage(&args[0]);
                process::exit(0);
            }
            other if input_path.is_none() && !other.starts_with('-') => {
                input_path = Some(other.to_string());
                i += 1;
            }
            _ => {
                print_usage(&args[0]);
                process::exit(1);
            }
        }
    }

    let raw_json = match input_path.as_deref() {
        None | Some("-") => {
            let mut buf = String::new();
            if let Err(e) = io::stdin().read_to_string(&mut buf) {
                eprintln!("Error reading stdin: {}", e);
                process::exit(1);
            }
            buf
        }
        Some(path) => match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading input file '{}': {}", path, e);
                process::exit(1);
            }
        },
    };

    let req: OptimizeRequestPayload = match serde_json::from_str(&raw_json) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error parsing JSON input: {}", e);
            process::exit(1);
        }
    };

    let resp = match run_optimization(&req) {
        Ok(res) => res,
        Err(e) => {
            let err_resp = serde_json::json!({
                "status": "error",
                "objective_value": null,
                "bottlenecks": [],
                "warnings": [],
                "error_msg": e.to_string(),
                "facts": [],
                "edits": []
            });
            let out_str = serde_json::to_string_pretty(&err_resp).unwrap();
            eprintln!("{}", out_str);
            process::exit(2);
        }
    };

    let out_json = match serde_json::to_string_pretty(&resp) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error serializing JSON output: {}", e);
            process::exit(1);
        }
    };

    match output_path.as_deref() {
        None | Some("-") => {
            let _ = io::stdout().write_all(out_json.as_bytes());
            let _ = io::stdout().write_all(b"\n");
        }
        Some(path) => {
            if let Err(e) = fs::write(path, out_json) {
                eprintln!("Error writing output file '{}': {}", path, e);
                process::exit(1);
            }
        }
    }
}
