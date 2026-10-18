//! Orchestration: scan, then for each image (bounded by a semaphore) build a
//! preview on the blocking pool, classify it, and place the original.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use tokio::{
    sync::{watch, Semaphore},
    task::{self, JoinError, JoinSet},
};

use crate::{
    api::{ApiError, ClientConfig, SubqClient},
    categories::{Categories, NEEDS_REVIEW, UNUSABLE},
    cli::Cli,
    credentials::{self, ApiKey, KeySource},
    preview::{self, PreviewOptions},
    report::{self, FileResult, Outcome, Review, Route, RunInfo, Stage, Thresholds, Totals},
    scan,
    sorter::{Sorter, Transfer},
    ui::{self, Paint, Progress, Tally},
    wizard::{self, Answer, Given, Setup},
};

const MAX_LISTED_FAILURES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Success,
    Fatal,
    PartialFailure,
    Interrupted,
}

impl From<RunStatus> for ExitCode {
    fn from(status: RunStatus) -> Self {
        ExitCode::from(match status {
            RunStatus::Success => 0,
            RunStatus::Fatal => 1,
            RunStatus::PartialFailure => 3,
            RunStatus::Interrupted => 130,
        })
    }
}

/// State shared by every per-image task.
struct Shared {
    client: SubqClient,
    categories: Categories,
    sorter: Sorter,
    preview: PreviewOptions,
    thresholds: Thresholds,
    /// Bounds concurrent decodes to the CPU count, independently of the
    /// (usually larger) number of in-flight HTTP requests, which keeps peak
    /// memory proportional to cores rather than to `--concurrency`.
    cpu: Semaphore,
    /// Set to stop the run (Ctrl-C or a fatal API error): nothing new is
    /// dispatched, and images that haven't reached the API yet are skipped.
    stop: watch::Sender<bool>,
    interrupted: AtomicBool,
    fatal: Mutex<Option<String>>,
    /// SubQ answered 401: the key itself is wrong or revoked.
    key_rejected: AtomicBool,
}

impl Shared {
    fn abort(&self, message: String) {
        let mut fatal = self.fatal.lock().unwrap_or_else(|p| p.into_inner());
        fatal.get_or_insert(message);
        drop(fatal);
        self.stop.send_replace(true);
    }

    fn stopping(&self) -> bool {
        *self.stop.borrow()
    }
}

pub async fn run(cli: Cli) -> Result<RunStatus> {
    // The key comes first so a first-time user is asked for it before
    // anything else, then the setup questions for whatever flags are missing.
    let api_key = resolve_api_key(cli.api_key.as_deref()).await?;
    let Some(setup) = resolve_setup(&cli).await? else {
        eprintln!("Cancelled; nothing was changed.");
        return Ok(RunStatus::Success);
    };
    let started = Instant::now();

    let copy = setup.copy;
    let categories = setup.categories;
    let thresholds = Thresholds {
        min_confidence: cli.min_confidence,
        unusable: setup.unusable.then_some(cli.unusable_threshold),
    };
    if thresholds.unusable.is_some() {
        if let Some(clash) = categories.find_folder(UNUSABLE) {
            bail!(
                "category {:?} has the same folder name as the one for unusable photos; \
                 rename the category, or leave unusable photos in their categories",
                clash.name
            );
        }
    }
    let input = resolve_input(&setup.input)?;
    let output = resolve_output(&setup.output)?;

    // The output may live inside the input (even be the same directory), so
    // never descend into destination folders: those files are already sorted.
    // That includes `unusable` from an earlier run, even with the check off.
    let exclude: Vec<PathBuf> = categories
        .iter()
        .map(|c| c.name.as_str())
        .chain([NEEDS_REVIEW, UNUSABLE])
        .map(|name| output.join(name))
        .collect();
    let scanned = {
        let root = input.clone();
        let recursive = cli.recursive;
        task::spawn_blocking(move || scan::scan(&root, recursive, &exclude))
            .await
            .context("directory scan failed")?
    };
    for warning in &scanned.warnings {
        eprintln!("{} {warning}", ui::warning_label());
    }

    let files = scanned.files;
    let total = files.len();
    if total == 0 {
        let hint = if cli.recursive {
            ""
        } else {
            " (add --recursive to include subfolders)"
        };
        eprintln!(
            "No images ({}) found in {}{hint}",
            scan::SUPPORTED_EXTENSIONS
                .iter()
                .map(|ext| format!(".{ext}"))
                .collect::<Vec<_>>()
                .join(", "),
            input.display()
        );
        return Ok(RunStatus::Success);
    }
    // The report is written last; finding out then that its path is wrong
    // would turn a run that already moved every file into a failure.
    if let Some(path) = &cli.report {
        report::check_writable(path)?;
    }

    let paint = Paint::stderr();
    let mode = match (cli.dry_run, copy) {
        (true, true) => "dry run (copy)",
        (true, false) => "dry run (move)",
        (false, true) => "copy",
        (false, false) => "move",
    };
    eprintln!(
        "{} {} · {total} image{} · {} categories{} · {mode} → {}",
        paint.bold("snapsort"),
        env!("CARGO_PKG_VERSION"),
        if total == 1 { "" } else { "s" },
        categories.len(),
        if thresholds.unusable.is_some() {
            format!(" + {UNUSABLE}")
        } else {
            String::new()
        },
        output.display()
    );

    let client = SubqClient::new(ClientConfig {
        url: cli.api_url.clone(),
        api_key: api_key.value,
        model: cli.model.clone(),
        timeout: Duration::from_secs(cli.timeout),
        max_retries: cli.retries,
        pool_size: cli.concurrency,
    })?;
    let cpu_slots = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, cli.concurrency);
    let (stop, mut stop_rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        client,
        categories,
        sorter: Sorter::new(
            output.clone(),
            if copy { Transfer::Copy } else { Transfer::Move },
            cli.dry_run,
        ),
        preview: PreviewOptions {
            max_dim: cli.max_dim,
            quality: cli.quality,
        },
        thresholds,
        cpu: Semaphore::new(cpu_slots),
        stop,
        interrupted: AtomicBool::new(false),
        fatal: Mutex::new(None),
        key_rejected: AtomicBool::new(false),
    });

    let progress = Progress::new(total as u64);
    spawn_interrupt_handler(shared.clone(), progress.clone());

    let permits = Arc::new(Semaphore::new(cli.concurrency));
    let mut tasks = JoinSet::new();
    let mut in_flight: HashMap<task::Id, (PathBuf, String)> = HashMap::new();
    let mut results: Vec<FileResult> = Vec::with_capacity(total);
    let mut tally = Tally::default();
    let mut pending = files.into_iter();
    let mut next = pending.next();

    loop {
        tokio::select! {
            biased;

            Some(joined) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                if let Some(result) = collect(joined, &mut in_flight) {
                    log_result(&progress, &result, &cli, &shared, &mut tally);
                    progress.inc();
                    results.push(result);
                }
                tally.in_flight = tasks.len();
                progress.update(tally);
            }

            _ = stop_rx.wait_for(|stop| *stop), if next.is_some() => {
                next = None;
            }

            permit = permits.clone().acquire_owned(), if next.is_some() => {
                let permit = permit.expect("semaphore is never closed");
                let source = next.take().expect("guarded by next.is_some()");
                let display = display_path(&source, &input);
                let task_shared = shared.clone();
                let task_source = source.clone();
                let task_display = display.clone();
                let handle = tasks.spawn(async move {
                    let _permit = permit;
                    let outcome = process(task_shared, &task_source).await?;
                    Some(FileResult { source: task_source, display: task_display, outcome })
                });
                in_flight.insert(handle.id(), (source, display));
                tally.in_flight = tasks.len();
                progress.update(tally);
                next = pending.next();
            }

            else => break,
        }
    }
    progress.finish();

    let fatal = shared
        .fatal
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    let interrupted = shared.interrupted.load(Ordering::SeqCst);
    results.sort_by(|a, b| a.display.cmp(&b.display));
    let totals = Totals::compute(&results, &shared.categories, total);
    let info = RunInfo {
        input: &input,
        output: &output,
        model: &cli.model,
        copy,
        dry_run: cli.dry_run,
        min_confidence: cli.min_confidence,
        unusable_threshold: thresholds.unusable,
        total,
        elapsed: started.elapsed(),
        interrupted,
        fatal: fatal.as_deref(),
    };

    let out = Paint::stdout();
    if cli.dry_run {
        println!("{}", report::render_plan(&results, thresholds, out));
    }
    print!("{}", report::render_summary(&totals, &info, out));
    // Under a fatal error every failure has the same cause; it's reported below.
    if fatal.is_none() {
        if let Some(failures) =
            report::render_failures(&results, MAX_LISTED_FAILURES, Paint::stderr())
        {
            eprintln!("\n{failures}");
        }
    }

    let mut status = if fatal.is_some() {
        RunStatus::Fatal
    } else if interrupted {
        RunStatus::Interrupted
    } else if totals.failed > 0 {
        RunStatus::PartialFailure
    } else {
        RunStatus::Success
    };

    if let Some(path) = &cli.report {
        match report::write_json(path, &results, &totals, &info) {
            Ok(()) => eprintln!("Report written to {}", path.display()),
            Err(err) => {
                eprintln!("{} {err:#}", ui::error_label());
                status = RunStatus::Fatal;
            }
        }
    }
    if let Some(message) = &fatal {
        eprintln!("\n{} {message}", ui::error_label());
        if shared.key_rejected.load(Ordering::SeqCst) {
            explain_rejected_key(&api_key.source);
        }
        if totals.not_processed > 0 {
            eprintln!(
                "Stopped early; {} image(s) were not processed.",
                totals.not_processed
            );
        }
    } else if interrupted && totals.not_processed > 0 {
        eprintln!(
            "Interrupted; {} image(s) were not processed.",
            totals.not_processed
        );
    }
    Ok(status)
}

/// Preview, classify and place one image. `None` if the run stopped before
/// the image reached the API: it is left alone and counts as not processed.
async fn process(shared: Arc<Shared>, source: &Path) -> Option<Outcome> {
    let file_name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let preview = {
        let _slot = shared
            .cpu
            .acquire()
            .await
            .expect("semaphore is never closed");
        // The wait for a slot can be long; don't decode for a stopped run.
        if shared.stopping() {
            return None;
        }
        let path = source.to_path_buf();
        let options = shared.preview;
        match task::spawn_blocking(move || preview::build(&path, options)).await {
            Ok(Ok(preview)) => preview,
            Ok(Err(err)) => return Some(Outcome::failed(Stage::Preview, format!("{err:#}"))),
            Err(err) => return Some(Outcome::failed(Stage::Preview, err.to_string())),
        }
    };
    // Check again after decoding: after a fatal error every request would
    // fail the same way (and may cost credits), and after Ctrl-C the user
    // wants out.
    if shared.stopping() {
        return None;
    }

    let decision = match shared
        .client
        .classify(
            &file_name,
            &preview.base64,
            &shared.categories,
            shared.thresholds.unusable.is_some(),
            shared.stop.subscribe(),
        )
        .await
    {
        Ok(decision) => decision,
        Err(ApiError::Cancelled) => return None,
        Err(err) => {
            if err.status() == Some(401) {
                shared.key_rejected.store(true, Ordering::SeqCst);
            }
            if err.is_fatal() {
                shared.abort(err.to_string());
            }
            return Some(Outcome::failed(Stage::Classify, err.to_string()));
        }
    };
    drop(preview);

    // An unusable photo goes to `unusable` whatever its category, so a
    // blurry shot the model couldn't place doesn't land in needs_review.
    let unusable = shared
        .thresholds
        .unusable
        .zip(decision.unusable)
        .is_some_and(|(threshold, probability)| probability >= threshold);
    let route = if unusable {
        Route::Unusable
    } else if !shared.categories.contains(&decision.choice) {
        Route::Review(Review::UnknownChoice)
    } else if decision.confidence < shared.thresholds.min_confidence {
        Route::Review(Review::LowConfidence)
    } else {
        Route::Category
    };
    let folder = match route {
        Route::Category => decision.choice.clone(),
        Route::Review(_) => NEEDS_REVIEW.to_owned(),
        Route::Unusable => UNUSABLE.to_owned(),
    };

    let placed = {
        let shared = shared.clone();
        let source = source.to_path_buf();
        let folder = folder.clone();
        task::spawn_blocking(move || shared.sorter.place(&source, &folder)).await
    };
    Some(match placed {
        Ok(Ok(destination)) => Outcome::Sorted {
            decision,
            folder,
            route,
            destination,
        },
        Ok(Err(err)) => Outcome::failed(Stage::Place, format!("{err:#}")),
        Err(err) => Outcome::failed(Stage::Place, err.to_string()),
    })
}

/// `None` for an image skipped because the run stopped.
fn collect(
    joined: Result<(task::Id, Option<FileResult>), JoinError>,
    in_flight: &mut HashMap<task::Id, (PathBuf, String)>,
) -> Option<FileResult> {
    match joined {
        Ok((id, result)) => {
            in_flight.remove(&id);
            result
        }
        Err(err) => {
            let (source, display) = in_flight.remove(&err.id()).unwrap_or_default();
            Some(FileResult {
                source,
                display,
                outcome: Outcome::failed(Stage::Internal, err.to_string()),
            })
        }
    }
}

fn log_result(
    progress: &Progress,
    result: &FileResult,
    cli: &Cli,
    shared: &Shared,
    tally: &mut Tally,
) {
    let paint = Paint::stderr();
    match &result.outcome {
        Outcome::Sorted {
            decision,
            folder,
            route: Route::Category,
            ..
        } => {
            tally.sorted += 1;
            if cli.verbose {
                progress.log(format!(
                    "{} {} → {folder} {}",
                    paint.green("✓"),
                    result.display,
                    paint.dim(&format!("({:.2})", decision.confidence))
                ));
            }
        }
        Outcome::Sorted {
            decision,
            folder,
            route: Route::Review(review),
            ..
        } => {
            tally.review += 1;
            if cli.verbose {
                progress.log(format!(
                    "{} {} → {folder} {}",
                    paint.yellow("?"),
                    result.display,
                    paint.dim(&format!(
                        "· {}",
                        report::review_note(decision, *review, shared.thresholds.min_confidence)
                    ))
                ));
            }
        }
        Outcome::Sorted {
            decision,
            folder,
            route: Route::Unusable,
            ..
        } => {
            tally.unusable += 1;
            if cli.verbose {
                progress.log(format!(
                    "{} {} → {folder} {}",
                    paint.yellow("~"),
                    result.display,
                    paint.dim(&format!(
                        "· {}",
                        report::unusable_note(
                            decision,
                            shared.thresholds.unusable.unwrap_or_default()
                        )
                    ))
                ));
            }
        }
        Outcome::Failed { stage, error } => {
            tally.failed += 1;
            // Once the run is aborting, every in-flight failure repeats the
            // same fatal error, so don't spam it per file.
            let aborting = shared.fatal.lock().map(|f| f.is_some()).unwrap_or(false);
            if !aborting {
                progress.log(format!(
                    "{} {} [{}] {error}",
                    paint.red("✗"),
                    result.display,
                    stage.as_str()
                ));
            }
        }
    }
}

fn spawn_interrupt_handler(shared: Arc<Shared>, progress: Progress) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        shared.interrupted.store(true, Ordering::SeqCst);
        shared.stop.send_replace(true);
        progress.log(format!(
            "{} interrupted: waiting for requests already sent (press Ctrl-C again to quit immediately)",
            ui::warning_label()
        ));
        if tokio::signal::ctrl_c().await.is_ok() {
            progress.finish();
            eprintln!("Aborted.");
            std::process::exit(130);
        }
    });
}

/// Folder, categories and destination from flags; in a terminal, ask for
/// any that are missing. `None` if the user cancelled at the confirmation.
async fn resolve_setup(cli: &Cli) -> Result<Option<Setup>> {
    if let (Some(input), Some(output), Some(categories)) =
        (&cli.input, &cli.output, &cli.categories)
    {
        return Ok(Some(Setup {
            input: input.clone(),
            output: output.clone(),
            categories: Categories::load(categories)?,
            copy: cli.copy,
            unusable: cli.unusable,
        }));
    }
    if !credentials::can_prompt() {
        let missing: Vec<&str> = [
            (cli.input.is_none(), "--input"),
            (cli.output.is_none(), "--output"),
            (cli.categories.is_none(), "--categories"),
        ]
        .into_iter()
        .filter_map(|(missing, flag)| missing.then_some(flag))
        .collect();
        bail!(
            "missing {}: pass them on the command line, or run snapsort in a terminal to be asked",
            missing.join(", ")
        );
    }
    let given = Given {
        input: cli.input.clone(),
        output: cli.output.clone(),
        categories: cli.categories.clone(),
        copy: cli.copy,
        dry_run: cli.dry_run,
        recursive: cli.recursive,
        unusable: cli.unusable,
    };
    let answer = task::spawn_blocking(move || wizard::run(&given))
        .await
        .context("setup prompt failed")??;
    Ok(match answer {
        Answer::Start(setup) => Some(setup),
        Answer::Cancelled => None,
    })
}

/// `--api-key` / `SUBQ_API_KEY` first, then the key saved by an earlier run,
/// then (only at a terminal) ask once and save it for next time.
async fn resolve_api_key(arg: Option<&str>) -> Result<ApiKey> {
    if let Some(key) = credentials::from_args(arg) {
        return Ok(key);
    }
    if let Some(key) = credentials::load_saved()? {
        return Ok(key);
    }
    if !credentials::can_prompt() {
        bail!(
            "missing SubQ API key: set the {} environment variable or pass --api-key \
             (or run snapsort once in a terminal to enter and save one)",
            credentials::ENV_VAR
        );
    }

    let value = task::spawn_blocking(credentials::prompt)
        .await
        .context("API key prompt failed")??
        .context("no API key entered")?;
    let source = match credentials::saved_key_path() {
        Some(path) => match credentials::save_to(&path, &value) {
            Ok(()) => {
                eprintln!(
                    "Saved to {} (delete it to change the key).\n",
                    path.display()
                );
                KeySource::Saved(path)
            }
            Err(err) => {
                eprintln!(
                    "{} {err:#}; using the key for this run only",
                    ui::warning_label()
                );
                KeySource::Prompt
            }
        },
        None => {
            eprintln!(
                "{} no config folder found (HOME is unset); using the key for this run only",
                ui::warning_label()
            );
            KeySource::Prompt
        }
    };
    Ok(ApiKey { value, source })
}

/// After a 401: say where the bad key came from, and drop a saved one so
/// the next run asks for a new key instead of failing the same way forever.
fn explain_rejected_key(source: &KeySource) {
    match source.saved_path() {
        Some(path) => match credentials::forget(path) {
            Ok(()) => eprintln!(
                "Removed the rejected key from {}; snapsort will ask for a new one next time.",
                path.display()
            ),
            Err(err) => eprintln!(
                "The rejected key is saved in {}; delete that file to enter a new one ({err}).",
                path.display()
            ),
        },
        None => eprintln!("The key came from {source}."),
    }
}

fn display_path(source: &Path, input: &Path) -> String {
    source
        .strip_prefix(input)
        .unwrap_or(source)
        .display()
        .to_string()
}

fn resolve_input(path: &Path) -> Result<PathBuf> {
    let resolved = fs::canonicalize(path)
        .with_context(|| format!("input directory {} does not exist", path.display()))?;
    if !resolved.is_dir() {
        bail!("input {} is not a directory", path.display());
    }
    Ok(simplify(resolved))
}

/// Canonicalize the output path even if it (or some parents) do not exist
/// yet, so it can be compared against canonical scan paths. Nothing is
/// created here; folders are created lazily when the first file is placed.
fn resolve_output(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("cannot resolve output path {}", path.display()))?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(canonical) = fs::canonicalize(existing) {
            if !canonical.is_dir() {
                bail!("output {} is not a directory", existing.display());
            }
            let mut resolved = canonical;
            resolved.extend(missing.iter().rev());
            return Ok(simplify(resolved));
        }
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
            bail!("cannot resolve output path {}", path.display());
        };
        missing.push(name.to_owned());
        existing = parent;
    }
}

/// Strip the `\\?\` verbatim prefix that `canonicalize` adds on Windows, for
/// readable output. A no-op elsewhere.
fn simplify(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_missing_output_under_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = fs::canonicalize(dir.path()).unwrap();
        let resolved = resolve_output(&dir.path().join("a/b")).unwrap();
        assert_eq!(resolved, simplify(canonical_root.join("a").join("b")));
        assert!(
            !dir.path().join("a").exists(),
            "resolve_output must not create folders"
        );
    }

    #[test]
    fn rejects_file_as_output_or_input() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, b"x").unwrap();
        assert!(resolve_output(&file).is_err());
        assert!(resolve_input(&file).is_err());
        assert!(resolve_input(&dir.path().join("missing")).is_err());
    }
}
