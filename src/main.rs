//! snapsort: classify and sort local images into folders with the SubQ Decisions API.

mod api;
mod app;
mod categories;
mod cli;
mod credentials;
mod preview;
mod raw;
mod report;
mod scan;
mod sorter;
mod ui;
mod wizard;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match app::run(cli).await {
        Ok(status) => status.into(),
        Err(err) => {
            eprintln!("{} {err:#}", ui::error_label());
            app::RunStatus::Fatal.into()
        }
    }
}
