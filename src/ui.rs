use std::{fmt, io::IsTerminal, time::Duration};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

/// Minimal ANSI styling that respects `NO_COLOR` and non-terminal output.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    enabled: bool,
}

impl Paint {
    pub fn stdout() -> Self {
        Self {
            enabled: color_allowed() && std::io::stdout().is_terminal(),
        }
    }

    pub fn stderr() -> Self {
        Self {
            enabled: color_allowed() && std::io::stderr().is_terminal(),
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.wrap("2", text)
    }

    pub fn green(self, text: &str) -> String {
        self.wrap("32", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.wrap("33", text)
    }

    pub fn red(self, text: &str) -> String {
        self.wrap("31", text)
    }

    fn wrap(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// Mirrors indicatif's rules (TERM unset/`dumb` disables the live UI on Unix)
/// so colors and progress bars are either both on or both off.
fn color_allowed() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let term = std::env::var("TERM").ok();
    let dumb = match term.as_deref() {
        Some("dumb") => true,
        None | Some("") => cfg!(unix),
        Some(_) => false,
    };
    !no_color && !dumb
}

pub fn error_label() -> String {
    Paint::stderr().red(&Paint::stderr().bold("error:"))
}

pub fn warning_label() -> String {
    Paint::stderr().yellow(&Paint::stderr().bold("warning:"))
}

/// Live status: a progress bar plus a tally line, drawn on stderr. Hidden
/// automatically when stderr is not a terminal (CI, pipes, log files).
#[derive(Clone)]
pub struct Progress {
    multi: MultiProgress,
    bar: ProgressBar,
    status: ProgressBar,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub sorted: usize,
    pub review: usize,
    pub unusable: usize,
    pub failed: usize,
    pub in_flight: usize,
}

impl Progress {
    pub fn new(total: u64) -> Self {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(12));
        let bar = multi.add(ProgressBar::new(total));
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.cyan} {elapsed_precise:.dim} {wide_bar:.cyan/blue} {pos}/{len} {rate:.dim} eta {eta}",
            )
            .expect("valid progress template")
            .with_key("rate", |state: &ProgressState, w: &mut dyn fmt::Write| {
                let _ = write!(w, "{:.1}/s", state.per_sec());
            })
            .progress_chars("━╸─")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        let status = multi.add(ProgressBar::new_spinner());
        status.set_style(ProgressStyle::with_template("  {msg}").expect("valid status template"));
        bar.enable_steady_tick(Duration::from_millis(100));
        let progress = Self { multi, bar, status };
        progress.update(Tally::default());
        progress
    }

    /// Print a line above the bars (or plainly to stderr when they are hidden).
    pub fn log(&self, line: impl AsRef<str>) {
        if self.multi.is_hidden() {
            eprintln!("{}", line.as_ref());
        } else {
            let _ = self.multi.println(line);
        }
    }

    pub fn update(&self, tally: Tally) {
        let unusable = if tally.unusable > 0 {
            format!(" · unusable {}", tally.unusable)
        } else {
            String::new()
        };
        self.status.set_message(format!(
            "sorted {} · needs review {}{unusable} · failed {} · in flight {}",
            tally.sorted, tally.review, tally.failed, tally.in_flight
        ));
    }

    pub fn inc(&self) {
        self.bar.inc(1);
    }

    pub fn finish(&self) {
        self.bar.finish_and_clear();
        self.status.finish_and_clear();
    }
}
