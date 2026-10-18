//! Per-file outcomes, the end-of-run summary, and the optional JSON report.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::{
    api::Decision,
    categories::{Categories, NEEDS_REVIEW, UNUSABLE},
    ui::Paint,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Preview,
    Classify,
    Place,
    Internal,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Preview => "preview",
            Stage::Classify => "classify",
            Stage::Place => "place",
            Stage::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    LowConfidence,
    /// The API answered with a label that is not one of our categories.
    UnknownChoice,
}

/// Which kind of folder an image went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The category the model chose.
    Category,
    /// `needs_review`.
    Review(Review),
    /// `unusable`: the photo is blurry, badly exposed or accidental.
    Unusable,
}

impl Route {
    pub fn status(self) -> &'static str {
        match self {
            Route::Category => "sorted",
            Route::Review(_) => "needs_review",
            Route::Unusable => "unusable",
        }
    }
}

#[derive(Debug)]
pub enum Outcome {
    Sorted {
        decision: Decision,
        folder: String,
        route: Route,
        destination: PathBuf,
    },
    Failed {
        stage: Stage,
        error: String,
    },
}

impl Outcome {
    pub fn failed(stage: Stage, error: impl Into<String>) -> Self {
        Outcome::Failed {
            stage,
            error: error.into(),
        }
    }
}

#[derive(Debug)]
pub struct FileResult {
    pub source: PathBuf,
    /// Source path relative to the input directory.
    pub display: String,
    pub outcome: Outcome,
}

pub struct RunInfo<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub model: &'a str,
    pub copy: bool,
    pub dry_run: bool,
    pub min_confidence: f64,
    /// Set when the unusable check is on.
    pub unusable_threshold: Option<f64>,
    pub total: usize,
    pub elapsed: Duration,
    pub interrupted: bool,
    pub fatal: Option<&'a str>,
}

#[derive(Debug, Default)]
pub struct Totals {
    pub sorted: usize,
    pub review: usize,
    pub unusable: usize,
    pub failed: usize,
    pub not_processed: usize,
    pub input_tokens: u64,
    /// Files per destination folder, in category order, then needs_review
    /// and unusable.
    pub per_folder: Vec<(String, usize)>,
}

impl Totals {
    pub fn compute(results: &[FileResult], categories: &Categories, total: usize) -> Self {
        let mut totals = Totals {
            not_processed: total.saturating_sub(results.len()),
            per_folder: categories
                .iter()
                .map(|c| (c.name.clone(), 0))
                .chain([(NEEDS_REVIEW.to_owned(), 0), (UNUSABLE.to_owned(), 0)])
                .collect(),
            ..Totals::default()
        };
        for result in results {
            match &result.outcome {
                Outcome::Sorted {
                    decision,
                    folder,
                    route,
                    ..
                } => {
                    match route {
                        Route::Category => totals.sorted += 1,
                        Route::Review(_) => totals.review += 1,
                        Route::Unusable => totals.unusable += 1,
                    }
                    totals.input_tokens += decision.input_tokens;
                    if let Some(entry) = totals
                        .per_folder
                        .iter_mut()
                        .find(|(name, _)| name == folder)
                    {
                        entry.1 += 1;
                    }
                }
                Outcome::Failed { .. } => totals.failed += 1,
            }
        }
        totals
    }
}

pub fn review_note(decision: &Decision, review: Review, min_confidence: f64) -> String {
    match review {
        Review::LowConfidence => {
            format!(
                "best guess {} ({:.2} < {min_confidence:.2})",
                decision.choice, decision.confidence
            )
        }
        Review::UnknownChoice => format!("model answered unknown folder {:?}", decision.choice),
    }
}

pub fn unusable_note(decision: &Decision, threshold: f64) -> String {
    format!(
        "likely unusable ({:.2} ≥ {threshold:.2}); otherwise {}",
        decision.unusable.unwrap_or_default(),
        decision.choice
    )
}

/// Note for an image's route, if it didn't simply go to its category.
fn route_note(decision: &Decision, route: Route, thresholds: Thresholds) -> Option<String> {
    match route {
        Route::Category => None,
        Route::Review(review) => Some(review_note(decision, review, thresholds.min_confidence)),
        Route::Unusable => Some(unusable_note(
            decision,
            thresholds.unusable.unwrap_or_default(),
        )),
    }
}

/// The thresholds that decided where images went.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub min_confidence: f64,
    /// Set when the unusable check is on.
    pub unusable: Option<f64>,
}

/// The per-file table printed by `--dry-run`. With the unusable check on, it
/// gets an UNUSABLE column, which helps when tuning `--unusable-threshold`.
pub fn render_plan(results: &[FileResult], thresholds: Thresholds, paint: Paint) -> String {
    let unusable_column = thresholds.unusable.is_some();
    let mut table = if unusable_column {
        Table::new(
            &["FILE", "FOLDER", "CONFIDENCE", "UNUSABLE", "NOTE"],
            &[false, false, true, true, false],
        )
    } else {
        Table::new(
            &["FILE", "FOLDER", "CONFIDENCE", "NOTE"],
            &[false, false, true, false],
        )
    };
    for result in results {
        match &result.outcome {
            Outcome::Sorted {
                decision,
                folder,
                route,
                destination,
            } => {
                let mut notes: Vec<String> = route_note(decision, *route, thresholds)
                    .into_iter()
                    .collect();
                let original = result.source.file_name();
                if destination.file_name() != original {
                    let renamed = destination
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    notes.push(format!("renamed to {renamed:?} (name taken)"));
                }
                let mut row = vec![
                    result.display.clone(),
                    folder.clone(),
                    format!("{:.2}", decision.confidence),
                ];
                if unusable_column {
                    row.push(decision.unusable.map_or("-".into(), |p| format!("{p:.2}")));
                }
                row.push(notes.join("; "));
                table.row(row);
            }
            Outcome::Failed { stage, error } => {
                let mut row = vec![result.display.clone(), "-".into(), "-".into()];
                if unusable_column {
                    row.push("-".into());
                }
                row.push(format!(
                    "failed ({}): {}",
                    stage.as_str(),
                    first_line(error)
                ));
                table.row(row);
            }
        }
    }
    table.render(paint)
}

pub fn render_summary(totals: &Totals, info: &RunInfo, paint: Paint) -> String {
    let mut out = String::new();
    let title = if info.dry_run {
        "Summary (dry run, no files were changed)"
    } else {
        "Summary"
    };
    out.push_str(&paint.bold(title));
    out.push('\n');

    let mut table = Table::new(&["FOLDER", "FILES"], &[false, true]);
    for (folder, count) in totals.per_folder.iter().filter(|(_, n)| *n > 0) {
        table.row(vec![folder.clone(), count.to_string()]);
    }
    if totals.failed > 0 {
        table.row(vec![
            "(failed, left in place)".into(),
            totals.failed.to_string(),
        ]);
    }
    if totals.not_processed > 0 {
        table.row(vec![
            "(not processed)".into(),
            totals.not_processed.to_string(),
        ]);
    }
    table.row(vec!["total".into(), info.total.to_string()]);
    out.push_str(&indent(&table.render(paint), "  "));

    let placed = totals.sorted + totals.review + totals.unusable;
    let verb = match (info.dry_run, info.copy) {
        (true, true) => "Would copy",
        (true, false) => "Would move",
        (false, true) => "Copied",
        (false, false) => "Moved",
    };
    out.push_str(&format!(
        "\n{} {placed} of {} image{} into {}",
        paint.green(verb),
        info.total,
        if info.total == 1 { "" } else { "s" },
        info.output.display()
    ));
    let mut details = Vec::new();
    if totals.review > 0 {
        details.push(paint.yellow(&format!("{} in {NEEDS_REVIEW}", totals.review)));
    }
    if totals.unusable > 0 {
        details.push(paint.yellow(&format!("{} in {UNUSABLE}", totals.unusable)));
    }
    if totals.failed > 0 {
        details.push(paint.red(&format!("{} failed", totals.failed)));
    }
    if totals.not_processed > 0 {
        details.push(paint.yellow(&format!("{} not processed", totals.not_processed)));
    }
    if !details.is_empty() {
        out.push_str(&format!(" ({})", details.join(", ")));
    }
    out.push_str(&paint.dim(&format!(
        "\n{} input tokens · {}\n",
        thousands(totals.input_tokens),
        human_duration(info.elapsed)
    )));
    out
}

/// Failure details for stderr, capped so a bad run doesn't flood the terminal.
pub fn render_failures(results: &[FileResult], limit: usize, paint: Paint) -> Option<String> {
    let failures: Vec<_> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            Outcome::Failed { stage, error } => Some((r, stage, error)),
            Outcome::Sorted { .. } => None,
        })
        .collect();
    if failures.is_empty() {
        return None;
    }
    let mut out = paint.red(&paint.bold(&format!(
        "{} file(s) failed and were left in place:",
        failures.len()
    )));
    for (result, stage, error) in failures.iter().take(limit) {
        out.push_str(&format!(
            "\n  {} [{}] {}",
            result.display,
            stage.as_str(),
            first_line(error)
        ));
    }
    if failures.len() > limit {
        out.push_str(&format!(
            "\n  … and {} more (use --report for the full list)",
            failures.len() - limit
        ));
    }
    Some(out)
}

/// Fail early if the report can't be written. Creates the file, empty, if it
/// doesn't exist yet; an existing report is left as it is until overwritten.
pub fn check_writable(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map(drop)
        .with_context(|| format!("cannot write report {}", path.display()))
}

pub fn write_json(
    path: &Path,
    results: &[FileResult],
    totals: &Totals,
    info: &RunInfo,
) -> Result<()> {
    let files: Vec<Value> = results.iter().map(file_json).collect();
    let report = json!({
        "snapsort_version": env!("CARGO_PKG_VERSION"),
        "model": info.model,
        "mode": if info.copy { "copy" } else { "move" },
        "dry_run": info.dry_run,
        "input": info.input,
        "output": info.output,
        "min_confidence": info.min_confidence,
        "unusable_threshold": info.unusable_threshold,
        "elapsed_seconds": (info.elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        "interrupted": info.interrupted,
        "fatal_error": info.fatal,
        "totals": {
            "images": info.total,
            "sorted": totals.sorted,
            "needs_review": totals.review,
            "unusable": totals.unusable,
            "failed": totals.failed,
            "not_processed": totals.not_processed,
            "input_tokens": totals.input_tokens,
        },
        "files": files,
    });
    let mut text = serde_json::to_string_pretty(&report)?;
    text.push('\n');
    fs::write(path, text).with_context(|| format!("failed to write report {}", path.display()))
}

fn file_json(result: &FileResult) -> Value {
    match &result.outcome {
        Outcome::Sorted {
            decision,
            folder,
            route,
            destination,
        } => {
            let probabilities: Map<String, Value> = decision
                .probabilities
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            json!({
                "source": result.source,
                "status": route.status(),
                "folder": folder,
                "destination": destination,
                "choice": decision.choice,
                "confidence": decision.confidence,
                "probabilities": probabilities,
                "unusable": decision.unusable,
                "review_reason": match route {
                    Route::Review(Review::LowConfidence) => Some("low_confidence"),
                    Route::Review(Review::UnknownChoice) => Some("unknown_choice"),
                    Route::Category | Route::Unusable => None,
                },
            })
        }
        Outcome::Failed { stage, error } => json!({
            "source": result.source,
            "status": "failed",
            "stage": stage.as_str(),
            "error": error,
        }),
    }
}

// ----- Formatting helpers ---------------------------------------------------

struct Table {
    headers: Vec<String>,
    right_align: Vec<bool>,
    rows: Vec<Vec<String>>,
}

impl Table {
    const MAX_CELL: usize = 64;

    fn new(headers: &[&str], right_align: &[bool]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            right_align: right_align.to_vec(),
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn render(&self, paint: Paint) -> String {
        let last = self.headers.len() - 1;
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        if i == last {
                            cell.clone()
                        } else {
                            ellipsize_start(cell, Self::MAX_CELL)
                        }
                    })
                    .collect()
            })
            .collect();
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|i| {
                cells
                    .iter()
                    .map(|row| width(&row[i]))
                    .chain([width(&self.headers[i])])
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let format_row = |row: &[String]| -> String {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                let pad = " ".repeat(widths[i].saturating_sub(width(cell)));
                if i > 0 {
                    line.push_str("  ");
                }
                if self.right_align[i] {
                    line.push_str(&pad);
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    if i != last {
                        line.push_str(&pad);
                    }
                }
            }
            line.trim_end().to_owned()
        };

        let mut out = paint.bold(&format_row(&self.headers));
        out.push('\n');
        for row in &cells {
            out.push_str(&format_row(row));
            out.push('\n');
        }
        out
    }
}

fn width(text: &str) -> usize {
    text.chars().count()
}

/// Keep the end of long paths, where the file name is.
fn ellipsize_start(text: &str, max: usize) -> String {
    let count = width(text);
    if count <= max {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}\n"))
        .collect()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = d.as_secs();
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_and_durations() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(human_duration(Duration::from_millis(14_250)), "14.2s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn ellipsizes_from_the_start() {
        assert_eq!(ellipsize_start("short", 10), "short");
        assert_eq!(ellipsize_start("a/very/long/path.jpg", 10), "…/path.jpg");
    }

    #[test]
    fn table_aligns_columns() {
        let mut table = Table::new(&["NAME", "N"], &[false, true]);
        table.row(vec!["cats".into(), "12".into()]);
        table.row(vec!["needs_review".into(), "3".into()]);
        let paint = Paint::stdout();
        let rendered = table.render(paint);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1], "cats          12");
        assert_eq!(lines[2], "needs_review   3");
    }
}
