use std::path::PathBuf;

use clap::Parser;

use crate::api::{DEFAULT_API_URL, DEFAULT_MODEL};

const AFTER_HELP: &str = "\
Run `snapsort` on its own to be asked for everything step by step.

Examples:
  snapsort
  snapsort -i ~/Pictures/inbox -o ~/Pictures/sorted -c categories.json
  snapsort -i ./photos -o ./photos -c categories.json --dry-run
  snapsort -i ./camera -o ./library -c categories.json --recursive --copy -j 64

Categories file (2-255 entries; keys become folder names, values are descriptions or null):
  {
    \"Livestock\": \"Ground-level photos where farm animals are the main subject\",
    \"Drone-Fields\": \"Aerial or drone photos of farmland, crops or paddocks\",
    \"People\": null
  }

Run without -c in a terminal and press Enter at the folders question to use the
built-in agriculture, crop and infrastructure folders.

Images whose confidence is below --min-confidence go to <output>/needs_review.
With --unusable, blurry, badly exposed or accidental photos go to <output>/unusable.

API key: --api-key or SUBQ_API_KEY if set; otherwise the first run in a terminal
asks for it and saves it (delete the saved file to change it).

-i, -o and -c are asked for when missing in a terminal, and required otherwise.

Exit codes: 0 success, 1 fatal error, 2 invalid arguments, 3 some files failed, 130 interrupted.";

/// Classify and sort images into folders using the SubQ Decisions API.
///
/// Each image is downscaled in memory (originals are never modified), sent to
/// SubQ for classification against your categories, then the original file is
/// moved (or copied) into the winning category folder.
#[derive(Debug, Parser)]
#[command(name = "snapsort", version, about, long_about, after_help = AFTER_HELP)]
pub struct Cli {
    /// Input directory containing unsorted images (.jpg, .jpeg, .png, .webp, or
    /// camera raw: .cr2, .cr3, .nef, .arw, .raf, .dng, .rw2)
    #[arg(short, long, value_name = "PATH")]
    pub input: Option<PathBuf>,

    /// Output root directory; category subfolders are created inside it
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// JSON file mapping folder names to descriptions (string or null)
    #[arg(short, long, value_name = "PATH")]
    pub categories: Option<PathBuf>,

    /// Maximum number of images processed in parallel
    #[arg(short = 'j', long, value_name = "N", default_value_t = 32, value_parser = parse_concurrency)]
    pub concurrency: usize,

    /// Maximum width/height in pixels of the in-memory preview sent to the API
    #[arg(long, value_name = "PX", default_value_t = 1280, value_parser = clap::value_parser!(u32).range(64..=16384))]
    pub max_dim: u32,

    /// JPEG quality (1-100) of the in-memory preview
    #[arg(long, value_name = "1-100", default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: u8,

    /// Confidence (0.0-1.0) below which files go to the needs_review folder
    #[arg(long, value_name = "0.0-1.0", default_value_t = 0.55, value_parser = parse_unit_interval)]
    pub min_confidence: f64,

    /// Put blurry, badly exposed or accidental photos in an `unusable` folder
    /// instead of their category folder
    #[arg(long)]
    pub unusable: bool,

    /// Likelihood (0.0-1.0) of being unusable at or above which photos go to the unusable folder
    #[arg(long, value_name = "0.0-1.0", default_value_t = 0.5, value_parser = parse_unit_interval, requires = "unusable")]
    pub unusable_threshold: f64,

    /// Copy files instead of moving them
    #[arg(long)]
    pub copy: bool,

    /// Classify and print where files would go, without touching the disk
    #[arg(long)]
    pub dry_run: bool,

    /// Scan the input directory recursively (default: top level only)
    #[arg(long)]
    pub recursive: bool,

    /// SubQ API key (if unset, you're asked once and it's saved)
    #[arg(long, value_name = "KEY", env = "SUBQ_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    /// Write a JSON report of every decision to this file
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Print one line per file as it is sorted
    #[arg(short, long)]
    pub verbose: bool,

    /// SubQ Decisions API endpoint
    #[arg(long, value_name = "URL", env = "SUBQ_API_URL", default_value = DEFAULT_API_URL, help_heading = "Advanced")]
    pub api_url: String,

    /// SubQ decision model
    #[arg(long, value_name = "MODEL", env = "SUBQ_MODEL", default_value = DEFAULT_MODEL, help_heading = "Advanced")]
    pub model: String,

    /// Retries per image for rate limits, server errors and network failures
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u32).range(0..=20), help_heading = "Advanced")]
    pub retries: u32,

    /// Per-request timeout in seconds
    #[arg(long, value_name = "SECS", default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600), help_heading = "Advanced")]
    pub timeout: u64,
}

fn parse_concurrency(raw: &str) -> Result<usize, String> {
    let value: usize = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a whole number"))?;
    if (1..=256).contains(&value) {
        Ok(value)
    } else {
        Err("must be between 1 and 256".into())
    }
}

fn parse_unit_interval(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err("must be between 0.0 and 1.0".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_spec() {
        let cli =
            Cli::try_parse_from(["snapsort", "-i", "in", "-o", "out", "-c", "c.json"]).unwrap();
        assert_eq!(cli.concurrency, 32);
        assert_eq!(cli.max_dim, 1280);
        assert_eq!(cli.quality, 80);
        assert_eq!(cli.min_confidence, 0.55);
        assert!(!cli.copy && !cli.dry_run && !cli.recursive && !cli.unusable);
        assert_eq!(cli.unusable_threshold, 0.5);
    }

    #[test]
    fn unusable_threshold_needs_unusable() {
        let base = ["snapsort", "-i", "in", "-o", "out", "-c", "c.json"];
        let with = |extra: &[&'static str]| Cli::try_parse_from(base.iter().chain(extra));
        assert!(with(&["--unusable-threshold", "0.8"]).is_err());
        let cli = with(&["--unusable", "--unusable-threshold", "0.8"]).unwrap();
        assert!(cli.unusable);
        assert_eq!(cli.unusable_threshold, 0.8);
    }

    #[test]
    fn bare_invocation_parses_for_interactive_setup() {
        let cli = Cli::try_parse_from(["snapsort"]).unwrap();
        assert!(cli.input.is_none() && cli.output.is_none() && cli.categories.is_none());
    }

    #[test]
    fn rejects_out_of_range_values() {
        let base = ["snapsort", "-i", "in", "-o", "out", "-c", "c.json"];
        for extra in [
            ["--quality", "0"],
            ["--quality", "101"],
            ["--min-confidence", "1.5"],
            ["--unusable-threshold", "-0.1"],
            ["-j", "0"],
        ] {
            let args = base.iter().chain(extra.iter());
            assert!(
                Cli::try_parse_from(args).is_err(),
                "{extra:?} should be rejected"
            );
        }
    }
}
