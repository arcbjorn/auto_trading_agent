//! The stored results: every report under `docs/results`, rendered on the page so the measured
//! numbers (throughput, accuracy per model, perturbed runs, the judge, the simulation) can be
//! read without opening a file or holding a key.

use super::App;
use super::html::{self, esc};
use super::markdown;
use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;

type Html = Response<Full<Bytes>>;

/// `(file name, title)` for every report, the index first.
fn reports(app: &App) -> Vec<(String, String)> {
    let mut names: Vec<String> = std::fs::read_dir(&app.results_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".md"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if let Some(i) = names.iter().position(|n| n == "README.md") {
        names.swap(0, i);
    }
    names
        .into_iter()
        .map(|n| {
            let title = std::fs::read_to_string(app.results_dir.join(&n))
                .ok()
                .and_then(|t| t.lines().find_map(|l| l.strip_prefix("# ").map(str::to_string)))
                .unwrap_or_else(|| n.clone());
            (n, title)
        })
        .collect()
}

pub fn section(app: &App) -> String {
    let list: String = reports(app)
        .iter()
        .map(|(name, title)| {
            // File stems tell the reports apart; several share the title "Evaluation report".
            format!(
                "<li><a href=\"#results\" title=\"{t}\" hx-get=\"/ui/results/{n}\" hx-target=\"#report\" hx-swap=\"innerHTML\" hx-on:click=\"document.querySelectorAll('#report-list a').forEach(function(a){{a.style.fontWeight=''}});this.style.fontWeight='700'\">{stem}</a></li>",
                n = esc(name),
                t = esc(title),
                stem = esc(name.trim_end_matches(".md"))
            )
        })
        .collect();
    let first = reports(app).first().map(|(n, _)| n.clone()).unwrap_or_default();
    let list_panel = html::panel(
        "Reports",
        "docs/results",
        "One file per recorded run, named by what produced it: <code>report-</code> for the scenario suites (agent, model, perturbation, judge), <code>sim-</code> for the simulation, <code>demo-</code> for the eight-turn transcript. The README lists every measurement with the report it comes from.",
        &format!("<ul class=\"list small free\" id=\"report-list\">{list}</ul>"),
    );
    let report_panel = html::panel(
        "Report",
        "rendered from the file",
        "The file as written by the harness, rendered by a small Markdown renderer. Pass rates come with a 95% Wilson interval; latency is per turn and per model call; tokens and cost are at list prices.",
        &format!(
            "<div id=\"report\" hx-get=\"/ui/results/{}\" hx-trigger=\"load\" hx-swap=\"innerHTML\"></div>",
            esc(&first)
        ),
    );
    html::tab(
        "results",
        "Results",
        "every measurement and the report behind it",
        "The recorded runs against real models, kept in the repository so every number in the README traces to a report written by the tools on this page.",
        "Throughput and soak numbers come from the benchmark binaries; accuracy, safety, robustness and the judge from the harness; P&amp;L from the simulation. Apple M1 Pro, release builds, loopback.",
        &format!("<div class=\"cols narrow-left\">{list_panel}{report_panel}</div>"),
        true,
    )
}

pub fn report(app: &App, name: &str) -> Html {
    // Only files listed from the results directory are served; a path with separators is refused.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") || !name.ends_with(".md") {
        return html::not_found();
    }
    match std::fs::read_to_string(app.results_dir.join(name)) {
        Ok(md) => html::html(markdown::render(&md)),
        Err(_) => html::not_found(),
    }
}
