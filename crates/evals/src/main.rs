//! `evals`: runs the scenario suites and the market simulation against the real stack.
//!
//!   evals run  [--suite execution|paraphrase|safety|all] [--reps N] [--agent model|oracle|null] [--cases DIR] [--out DIR]
//!   evals sim  [--seeds N] [--rounds R] [--agent model|baseline|null] [--out DIR]
//!
//! `--agent model` calls the Messages API and needs ANTHROPIC_API_KEY. `oracle` performs the
//! expected actions directly (it must score 100% on execution) and `null` does nothing (it must
//! score 0% on execution and 100% on attacks); both validate the harness without a model.
mod agents;
mod cases;
mod harness;
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
    };
    let mut it = std::env::args().skip(1);
    if let Some(cmd) = it.next() {
        args.command = cmd;
    }
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_default();
        match flag.as_str() {
            "--suite" => args.suite = value,
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
        "report" => report::regenerate(&args.out_dir),
        other => anyhow::bail!("unknown command {other}; use run, sim or report"),
    }
}
