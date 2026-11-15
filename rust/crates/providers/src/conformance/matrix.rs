//! Matrix report: per model and toolset success, calls per task, outcome classes, qualification.
use super::check::{class_name, is_invalid};
use super::runner::RunOutcome;
use serde::Serialize;
use std::collections::BTreeMap;

/// Qualification pass mark (decision OD7); configurable.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Thresholds {
    pub min_success: f64,
    pub max_invalid: f64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_success: 0.90,
            max_invalid: 0.05,
        }
    }
}
impl Thresholds {
    /// success >= min_success and invalid <= max_invalid, with a tolerance for float rounding.
    pub fn qualifies(&self, success: f64, invalid: f64) -> bool {
        const EPS: f64 = 1e-9;
        success + EPS >= self.min_success && invalid <= self.max_invalid + EPS
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cell {
    pub model: String,
    pub toolset: String,
    pub tasks: u32,
    pub runs: u32,
    pub successes: u32,
    pub success_rate: f64,
    pub calls: u32,
    pub calls_per_task: f64,
    pub outcome_classes: BTreeMap<String, u32>,
    pub invalid_calls: u32,
    pub invalid_rate: f64,
    pub qualified: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Matrix {
    pub thresholds: Thresholds,
    pub cells: Vec<Cell>,
}

fn ratio(part: u32, whole: u32) -> f64 {
    if whole == 0 {
        0.0
    } else {
        f64::from(part) / f64::from(whole)
    }
}

/// Folds run outcomes (one list per model) into cells, ordered by model then toolset.
pub fn build_matrix(
    outcomes: Vec<(String, Vec<RunOutcome>)>,
    runs: u32,
    thresholds: Thresholds,
) -> Matrix {
    let mut cells = Vec::new();
    for (model, model_outcomes) in outcomes {
        let mut by_toolset: BTreeMap<&str, Vec<&RunOutcome>> = BTreeMap::new();
        for outcome in &model_outcomes {
            by_toolset.entry(&outcome.toolset).or_default().push(outcome);
        }
        for (toolset, set) in by_toolset {
            let task_runs = u32::try_from(set.len()).unwrap_or(u32::MAX);
            let successes = u32::try_from(set.iter().filter(|o| o.success).count()).unwrap_or(0);
            let mut classes = BTreeMap::new();
            let (mut calls, mut invalid) = (0u32, 0u32);
            for call in set.iter().flat_map(|o| &o.calls) {
                calls += 1;
                invalid += u32::from(is_invalid(call.class));
                *classes.entry(class_name(call.class)).or_insert(0) += 1;
            }
            let success_rate = ratio(successes, task_runs);
            let invalid_rate = ratio(invalid, calls);
            cells.push(Cell {
                model: model.clone(),
                toolset: toolset.to_owned(),
                tasks: if runs == 0 { 0 } else { task_runs / runs },
                runs,
                successes,
                success_rate,
                calls,
                calls_per_task: ratio(calls, task_runs),
                outcome_classes: classes,
                invalid_calls: invalid,
                invalid_rate,
                qualified: task_runs > 0 && thresholds.qualifies(success_rate, invalid_rate),
            });
        }
    }
    Matrix { thresholds, cells }
}

impl Matrix {
    pub fn cell(&self, model: &str, toolset: &str) -> Option<&Cell> {
        self.cells
            .iter()
            .find(|c| c.model == model && c.toolset == toolset)
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
    pub fn to_markdown(&self) -> String {
        let t = &self.thresholds;
        let mut out = format!(
            "# Conformance matrix\n\nQualified: success >= {:.2} and invalid <= {:.2}\n\n\
             | Model | Toolset | Tasks x runs | Success | Calls/task | Invalid | Qualified | Outcome classes |\n\
             |---|---|---|---|---|---|---|---|\n",
            t.min_success, t.max_invalid
        );
        for c in &self.cells {
            let classes: Vec<String> = c
                .outcome_classes
                .iter()
                .map(|(name, n)| format!("{name}={n}"))
                .collect();
            out.push_str(&format!(
                "| {} | {} | {}x{} | {:.2} | {:.2} | {:.2} | {} | {} |\n",
                c.model,
                c.toolset,
                c.tasks,
                c.runs,
                c.success_rate,
                c.calls_per_task,
                c.invalid_rate,
                if c.qualified { "yes" } else { "no" },
                classes.join(", ")
            ));
        }
        out
    }
}
