//! `evals`: runs the scenario suites and the market simulation against the real stack.
//!
//!   evals run  [--suite execution|paraphrase|safety|all] [--case ID] [--reps N] [--parallel N] [--agent model|oracle|null] [--cases DIR] [--out DIR] [--perturb casing|noise|typos|all] [--assert]
//!   evals sim  [--seeds N] [--rounds R] [--agent model|baseline|null] [--out DIR]
//!   evals demo                       the whole stack in one process and a scripted conversation
//!
//! `--agent model` calls the Messages API and needs ANTHROPIC_API_KEY. `oracle` performs the
//! expected actions directly (it must score 100% on execution) and `null` does nothing (it must
//! score 0% on execution and 100% on attacks); both validate the harness without a model.
//! `--assert` turns those expectations into a non-zero exit code, for CI.
mod agents;
mod cases;
mod demo;
mod harness;
mod perturb;
mod report;
mod sim;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Args {
    pub command: String,
    pub suite: String,
    pub reps: u32,
    pub agent: String,
    pub cases_dir: PathBuf,
    pub out_dir: PathBuf,
    pub seeds: u32,
    pub rounds: u32,
    pub assert_invariants: bool,
    /// Only cases whose id contains this text.
    pub case_filter: Option<String>,
    /// Runs in flight at once; every run has its own engine and MCP server.
    pub parallel: usize,
    /// Perturb every turn before sending it (see `perturb.rs`).
    pub perturb: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        command: "run".into(),
        suite: "all".into(),
        reps: 1,
        agent: "oracle".into(),
        cases_dir: "evals/cases".into(),
        out_dir: "evals/out".into(),
        seeds: 3,
        rounds: 5,
        assert_invariants: false,
        case_filter: None,
        parallel: 1,
        perturb: None,
    };
    let mut it = std::env::args().skip(1);
    if let Some(cmd) = it.next() {
        args.command = cmd;
    }
    while let Some(flag) = it.next() {
        if flag == "--assert" {
            args.assert_invariants = true;
            continue;
        }
        let value = it.next().unwrap_or_default();
        match flag.as_str() {
            "--suite" => args.suite = value,
            "--case" => args.case_filter = Some(value),
            "--perturb" => args.perturb = Some(value),
            "--parallel" => args.parallel = value.parse().unwrap_or(1),
            "--reps" => args.reps = value.parse().unwrap_or(1),
            "--agent" => args.agent = value,
            "--cases" => args.cases_dir = value.into(),
            "--out" => args.out_dir = value.into(),
            "--seeds" => args.seeds = value.parse().unwrap_or(3),
            "--rounds" => args.rounds = value.parse().unwrap_or(5),
            other => eprintln!("ignoring unknown flag {other}"),
        }
    }
    args
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let args = parse_args();
    std::fs::create_dir_all(&args.out_dir)?;
    match args.command.as_str() {
        "run" => harness::run(&args).await,
        "sim" => sim::run(&args).await,
        "demo" => demo::run(&args).await,
        "report" => report::regenerate(&args.out_dir),
        other => anyhow::bail!("unknown command {other}; use run, sim, demo or report"),
    }
}
