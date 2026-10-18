//! End-to-end tests: run the real binary against a local mock of the SubQ
//! Decisions API that validates every request against the documented schema.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Output,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{DynamicImage, GenericImageView, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

#[path = "support/raw.rs"]
mod raw;

const CATEGORIES: &str = r#"{
    "cats": "Photos where a cat is the main subject",
    "dogs": "Photos where a dog is the main subject",
    "screenshots": null
}"#;

// ----- Mock server ----------------------------------------------------------

struct Reply {
    status: u16,
    body: Value,
    retry_after: Option<&'static str>,
}

impl Reply {
    fn choice(choice: &str, confidence: f64) -> Self {
        let other = 1.0 - confidence;
        Reply {
            status: 200,
            body: json!({
                "model": "subq-snap-preview",
                "answers": { "target_folder": {
                    "type": "choice",
                    "choice": choice,
                    "confidence": confidence,
                    "probabilities": { choice: confidence, "screenshots": other }
                }},
                "usage": { "input_tokens": 250, "output_tokens": 0 }
            }),
            retry_after: None,
        }
    }

    fn error(status: u16, error_type: &str, message: &str) -> Self {
        Reply {
            status,
            body: json!({ "detail": { "error_type": error_type, "message": message } }),
            retry_after: None,
        }
    }
}

type Handler = Arc<dyn Fn(&str, usize) -> Reply + Send + Sync>;

#[derive(Clone)]
struct Recorded {
    authorization: Option<String>,
    body: Value,
}

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Mock {
    /// `handler(file_name, attempt)` decides the reply; attempt counts from 0
    /// per file name so tests can script retries.
    async fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (handler, recorded, attempts) =
                    (handler.clone(), recorded.clone(), attempts.clone());
                tokio::spawn(async move {
                    serve(socket, handler, recorded, attempts).await;
                });
            }
        });
        Mock { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn files_requested(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .requests()
            .iter()
            .map(|r| file_name_of(&r.body))
            .collect();
        names.sort();
        names
    }
}

async fn serve(
    mut socket: TcpStream,
    handler: Handler,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    attempts: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let length: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body: Value =
        serde_json::from_slice(&buf[header_end..header_end + length]).unwrap_or(Value::Null);
    let file = file_name_of(&body);
    let asks_unusable = body_asks_unusable(&body);
    let attempt = {
        let mut attempts = attempts.lock().unwrap();
        let entry = attempts.entry(file.clone()).or_default();
        *entry += 1;
        *entry - 1
    };
    recorded.lock().unwrap().push(Recorded {
        authorization: header("authorization"),
        body,
    });

    let mut reply = handler(&file, attempt);
    if reply.status == 200 && asks_unusable {
        reply.body["answers"]["unusable"] = json!({
            "type": "noul",
            "noul": unusable_probability(&file),
        });
    }
    let payload = reply.body.to_string();
    let mut response = format!(
        "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        payload.len()
    );
    if let Some(retry_after) = reply.retry_after {
        response.push_str(&format!("Retry-After: {retry_after}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&payload);
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn file_name_of(body: &Value) -> String {
    body["state"]
        .as_str()
        .and_then(|s| s.strip_prefix("Classify file: "))
        .unwrap_or("<invalid state>")
        .to_owned()
}

fn body_asks_unusable(body: &Value) -> bool {
    body["questions"].get("unusable").is_some()
}

/// How likely the mock model thinks a photo is to be unusable, by file name.
fn unusable_probability(file: &str) -> f64 {
    if file.contains("shaky") {
        0.97
    } else if file.contains("borderline") {
        0.45
    } else {
        0.04
    }
}

/// Replies based on the file name prefix, like a well-behaved model would.
fn by_name() -> Handler {
    Arc::new(|file: &str, attempt: usize| match file {
        f if f.starts_with("cat") => Reply::choice("cats", 0.93),
        f if f.starts_with("dog") => Reply::choice("dogs", 0.88),
        f if f.starts_with("blurry") => Reply::choice("dogs", 0.31),
        f if f.starts_with("alien") => Reply::choice("aliens", 0.99),
        f if f.starts_with("reject") => {
            Reply::error(422, "validation_error", "image could not be processed")
        }
        f if f.starts_with("flaky") && attempt == 0 => Reply {
            retry_after: Some("0"),
            ..Reply::error(503, "unavailable", "try again")
        },
        _ => Reply::choice("cats", 0.9),
    })
}

/// Assert a request body matches the SubQ contract, returning the decoded preview.
fn assert_valid_request(request: &Recorded, max_dim: u32) -> DynamicImage {
    assert_eq!(request.authorization.as_deref(), Some("Bearer test-key"));
    let body = &request.body;
    assert_eq!(body["model"], "subq-snap-preview");
    assert!(body["state"]
        .as_str()
        .unwrap()
        .starts_with("Classify file: "));
    let images = body["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["mediaType"], "image/jpeg");
    let b64 = images[0]["base64"].as_str().unwrap();
    assert!(
        !b64.starts_with("data:") && !b64.contains('\n'),
        "base64 must be raw and single-line"
    );
    let jpeg = STANDARD.decode(b64).expect("valid base64");
    assert!(jpeg.len() < 4 * 1024 * 1024, "preview must be under 4 MiB");
    assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "preview must be a JPEG");

    let question = &body["questions"]["target_folder"];
    assert_eq!(question["type"], "choice");
    assert_eq!(
        question["instructions"],
        "Select the single best folder category for this image."
    );
    assert_eq!(
        question["criteria"],
        json!({
            "cats": "Photos where a cat is the main subject",
            "dogs": "Photos where a dog is the main subject",
            "screenshots": null
        })
    );
    let questions = body["questions"].as_object().unwrap();
    if let Some(unusable) = questions.get("unusable") {
        assert_eq!(unusable["type"], "noul");
        assert!(!unusable["instructions"].as_str().unwrap().is_empty());
        let criteria = unusable["criteria"].as_object().unwrap();
        assert_eq!(criteria.len(), 2, "noul criteria are `true` and `false`");
        assert!(criteria["true"].is_string() && criteria["false"].is_string());
    }
    assert_eq!(
        questions.len(),
        1 + usize::from(questions.contains_key("unusable")),
        "no unexpected questions"
    );
    assert_eq!(
        body.as_object().unwrap().len(),
        4,
        "no unexpected top-level fields"
    );

    let preview = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).unwrap();
    let (w, h) = preview.dimensions();
    assert!(
        w <= max_dim && h <= max_dim,
        "preview {w}x{h} exceeds {max_dim}"
    );
    preview
}

// ----- Fixtures -------------------------------------------------------------

struct Workspace {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join("categories.json"), CATEGORIES).unwrap();
        fs::create_dir_all(root.join("in")).unwrap();
        Workspace { _dir: dir, root }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.root.join(rel)
    }

    fn str(&self, rel: &str) -> String {
        self.path(rel).to_string_lossy().into_owned()
    }

    fn jpeg(&self, rel: &str, width: u32, height: u32) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let img = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        DynamicImage::ImageRgb8(img)
            .save_with_format(path, ImageFormat::Jpeg)
            .unwrap();
    }

    fn png(&self, rel: &str) {
        let img = RgbaImage::from_fn(640, 480, |x, _| Rgba([200, 30, 30, (x % 256) as u8]));
        DynamicImage::ImageRgba8(img)
            .save_with_format(self.path(rel), ImageFormat::Png)
            .unwrap();
    }

    fn webp(&self, rel: &str) {
        let img = RgbImage::from_fn(320, 240, |x, y| Rgb([x as u8, y as u8, 7]));
        DynamicImage::ImageRgb8(img)
            .save_with_format(self.path(rel), ImageFormat::WebP)
            .unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    async fn run(&self, mock: &Mock, extra: &[&str]) -> Output {
        let mut args = vec![
            "-i".to_owned(),
            self.str("in"),
            "-o".to_owned(),
            self.str("out"),
            "-c".to_owned(),
            self.str("categories.json"),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        run_raw(
            &args,
            Some(&mock.url),
            Some("test-key"),
            &self.path("config"),
        )
        .await
    }

    /// Like `run`, but with no key in the flags or environment.
    async fn run_without_key(&self, mock: &Mock) -> Output {
        let args: Vec<String> = ["-i", "in", "-o", "out", "-c", "categories.json"]
            .iter()
            .map(|a| {
                if a.starts_with('-') {
                    (*a).to_owned()
                } else {
                    self.str(a)
                }
            })
            .collect();
        run_raw(&args, Some(&mock.url), None, &self.path("config")).await
    }

    fn save_key(&self, key: &str) {
        fs::create_dir_all(self.path("config")).unwrap();
        fs::write(
            self.path("config/credentials.json"),
            json!({ "api_key": key }).to_string(),
        )
        .unwrap();
    }
}

async fn run_raw(
    args: &[String],
    api_url: Option<&str>,
    api_key: Option<&str>,
    config_dir: &Path,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_snapsort"));
    // Never read or overwrite a real saved key on the machine running tests.
    cmd.args(args)
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("SNAPSORT_CONFIG_DIR", config_dir)
        .env_remove("SUBQ_MODEL");
    match api_url {
        Some(url) => cmd.env("SUBQ_API_URL", url),
        None => cmd.env_remove("SUBQ_API_URL"),
    };
    match api_key {
        Some(key) => cmd.env("SUBQ_API_KEY", key),
        None => cmd.env_remove("SUBQ_API_KEY"),
    };
    cmd.output().await.unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn debug(output: &Output) -> String {
    format!(
        "status: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        stdout(output),
        stderr(output)
    )
}

// ----- Tests ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn moves_images_and_routes_low_confidence_to_review() {
    let ws = Workspace::new();
    ws.jpeg("in/cat_big.jpg", 3000, 2000);
    ws.png("in/dog_alpha.png");
    ws.webp("in/blurry.webp");
    ws.jpeg("in/alien.jpeg", 64, 64);
    fs::write(ws.path("in/notes.txt"), "not an image").unwrap();
    let originals: Vec<Vec<u8>> = ["cat_big.jpg", "dog_alpha.png", "blurry.webp", "alien.jpeg"]
        .iter()
        .map(|f| ws.read(&format!("in/{f}")))
        .collect();

    let mock = Mock::start(by_name()).await;
    let output = ws.run(&mock, &[]).await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));

    assert_eq!(
        ws.read("out/cats/cat_big.jpg"),
        originals[0],
        "original bytes must be untouched"
    );
    assert_eq!(ws.read("out/dogs/dog_alpha.png"), originals[1]);
    assert_eq!(ws.read("out/needs_review/blurry.webp"), originals[2]);
    assert_eq!(
        ws.read("out/needs_review/alien.jpeg"),
        originals[3],
        "unknown choices go to review"
    );
    assert!(ws.exists("in/notes.txt"));
    assert!(!ws.exists("in/cat_big.jpg"));
    assert!(
        !ws.exists("out/screenshots"),
        "folders are only created when used"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert!(
            !body_asks_unusable(&request.body),
            "the unusable check is opt-in"
        );
        let preview = assert_valid_request(request, 1280);
        if file_name_of(&request.body) == "cat_big.jpg" {
            assert_eq!(preview.dimensions(), (1280, 853), "aspect ratio preserved");
        }
    }

    let summary = stdout(&output);
    assert!(summary.contains("Moved 4 of 4 images"), "{summary}");
    assert!(summary.contains("2 in needs_review"), "{summary}");
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_mode_keeps_originals() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 200, 100);
    ws.jpeg("in/dog.jpg", 200, 100);
    let mock = Mock::start(by_name()).await;

    let output = ws
        .run(&mock, &["--copy", "--max-dim", "128", "--quality", "50"])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));
    assert_eq!(ws.read("in/cat.jpg"), ws.read("out/cats/cat.jpg"));
    assert_eq!(ws.read("in/dog.jpg"), ws.read("out/dogs/dog.jpg"));
    for request in mock.requests() {
        assert_valid_request(&request, 128);
    }
    assert!(stdout(&output).contains("Copied 2 of 2 images"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_prints_plan_without_touching_disk() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 100, 100);
    ws.jpeg("in/blurry.jpg", 100, 100);
    let mock = Mock::start(by_name()).await;

    let output = ws
        .run(&mock, &["--dry-run", "--min-confidence", "0.5"])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));
    assert!(ws.exists("in/cat.jpg") && ws.exists("in/blurry.jpg"));
    assert!(
        !ws.exists("out"),
        "dry run must not create the output directory"
    );

    let plan = stdout(&output);
    let cat_row = plan
        .lines()
        .find(|l| l.starts_with("cat.jpg"))
        .expect(&plan);
    assert!(
        cat_row.contains("cats") && cat_row.contains("0.93"),
        "{cat_row}"
    );
    let blurry_row = plan
        .lines()
        .find(|l| l.starts_with("blurry.jpg"))
        .expect(&plan);
    assert!(
        blurry_row.contains("needs_review") && blurry_row.contains("best guess dogs"),
        "{blurry_row}"
    );
    assert!(plan.contains("Would move 2 of 2 images"), "{plan}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unusable_photos_leave_their_categories() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 100, 100);
    ws.jpeg("in/cat_shaky.jpg", 100, 100);
    ws.jpeg("in/cat_borderline.jpg", 100, 100);
    // Low category confidence too: unusable wins over needs_review.
    ws.jpeg("in/blurry_shaky.jpg", 100, 100);
    ws.jpeg("in/blurry.jpg", 100, 100);
    let shaky = ws.read("in/cat_shaky.jpg");
    let report_path = ws.str("report.json");
    let mock = Mock::start(by_name()).await;

    let output = ws
        .run(&mock, &["--unusable", "--report", &report_path])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));

    assert_eq!(ws.read("out/unusable/cat_shaky.jpg"), shaky);
    assert!(ws.exists("out/unusable/blurry_shaky.jpg"));
    assert!(
        ws.exists("out/cats/cat_borderline.jpg"),
        "0.45 is below the default threshold of 0.5"
    );
    assert!(ws.exists("out/cats/cat.jpg"));
    assert!(ws.exists("out/needs_review/blurry.jpg"));
    assert!(!ws.exists("out/cats/cat_shaky.jpg"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert!(
            body_asks_unusable(&request.body),
            "one request, both questions"
        );
        assert_valid_request(request, 1280);
    }

    let summary = stdout(&output);
    assert!(summary.contains("Moved 5 of 5 images"), "{summary}");
    assert!(summary.contains("2 in unusable"), "{summary}");
    assert!(summary.contains("1 in needs_review"), "{summary}");

    let report: Value = serde_json::from_str(&fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["unusable_threshold"], 0.5);
    assert_eq!(report["totals"]["unusable"], 2);
    assert_eq!(report["totals"]["sorted"], 2);
    assert_eq!(report["totals"]["needs_review"], 1);
    let shaky_entry = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["source"].as_str().unwrap().ends_with("cat_shaky.jpg"))
        .unwrap();
    assert_eq!(shaky_entry["status"], "unusable");
    assert_eq!(shaky_entry["folder"], "unusable");
    assert_eq!(shaky_entry["choice"], "cats");
    assert_eq!(shaky_entry["unusable"], 0.97);
    assert_eq!(shaky_entry["review_reason"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_shows_unusable_scores_for_tuning_the_threshold() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 100, 100);
    ws.jpeg("in/cat_borderline.jpg", 100, 100);
    let mock = Mock::start(by_name()).await;

    let output = ws
        .run(
            &mock,
            &["--dry-run", "--unusable", "--unusable-threshold", "0.4"],
        )
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));
    assert!(!ws.exists("out"));

    let plan = stdout(&output);
    assert!(plan.lines().any(|l| l.contains("UNUSABLE")), "{plan}");
    let row = |name: &str| {
        plan.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row in\n{plan}"))
            .to_owned()
    };
    let borderline = row("cat_borderline.jpg");
    assert!(
        borderline.contains("unusable")
            && borderline.contains("0.45 ≥ 0.40")
            && borderline.contains("otherwise cats"),
        "{borderline}"
    );
    let cat = row("cat.jpg ");
    assert!(cat.contains("cats") && cat.contains("0.04"), "{cat}");
    assert!(plan.contains("1 in unusable"), "{plan}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_category_clashes_with_the_unusable_folder() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    fs::write(
        ws.path("categories.json"),
        r#"{"cats": null, "Unusable": "Bad photos"}"#,
    )
    .unwrap();
    let mock = Mock::start(by_name()).await;

    let output = ws.run(&mock, &["--unusable"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
    assert!(
        stderr(&output).contains("\"Unusable\" has the same folder name"),
        "{}",
        debug(&output)
    );
    assert!(mock.requests().is_empty() && ws.exists("in/cat.jpg"));

    // Without --unusable the category is an ordinary folder.
    let output = ws.run(&mock, &[]).await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_transient_errors_and_isolates_bad_files() {
    let ws = Workspace::new();
    ws.jpeg("in/flaky.jpg", 100, 100);
    ws.jpeg("in/reject.jpg", 100, 100);
    ws.jpeg("in/cat.jpg", 100, 100);
    fs::write(ws.path("in/broken.png"), b"\x89PNG but not really").unwrap();
    let report_path = ws.str("report.json");
    let mock = Mock::start(by_name()).await;

    let output = ws.run(&mock, &["--report", &report_path]).await;
    assert_eq!(
        output.status.code(),
        Some(3),
        "partial failure exit code\n{}",
        debug(&output)
    );
    assert!(
        ws.exists("out/cats/flaky.jpg"),
        "503 should be retried\n{}",
        debug(&output)
    );
    assert!(ws.exists("out/cats/cat.jpg"));
    assert!(
        ws.exists("in/reject.jpg") && ws.exists("in/broken.png"),
        "failed files stay in place"
    );

    let err = stderr(&output);
    assert!(
        err.contains("reject.jpg") && err.contains("image could not be processed"),
        "{err}"
    );
    assert!(
        err.contains("broken.png") && err.contains("decode"),
        "{err}"
    );
    assert_eq!(
        mock.files_requested(),
        ["cat.jpg", "flaky.jpg", "flaky.jpg", "reject.jpg"]
    );

    let report: Value = serde_json::from_str(&fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["totals"]["sorted"], 2);
    assert_eq!(report["totals"]["failed"], 2);
    assert_eq!(report["totals"]["input_tokens"], 500);
    let statuses: HashMap<String, (String, String)> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let name = Path::new(f["source"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let stage = f["stage"].as_str().unwrap_or("").to_owned();
            (name, (f["status"].as_str().unwrap().to_owned(), stage))
        })
        .collect();
    assert_eq!(statuses["broken.png"], ("failed".into(), "preview".into()));
    assert_eq!(statuses["reject.jpg"], ("failed".into(), "classify".into()));
    assert_eq!(statuses["flaky.jpg"].0, "sorted");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_failure_aborts_the_run() {
    let ws = Workspace::new();
    for i in 0..6 {
        ws.jpeg(&format!("in/cat_{i}.jpg"), 64, 64);
    }
    let mock = Mock::start(Arc::new(|_: &str, _| {
        Reply::error(401, "authentication_error", "invalid api key")
    }))
    .await;

    let output = ws.run(&mock, &["-j", "1"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
    let err = stderr(&output);
    assert!(err.contains("401") && err.contains("SUBQ_API_KEY"), "{err}");
    assert!(err.contains("not processed"), "{err}");
    assert_eq!(
        mock.requests().len(),
        1,
        "no further requests after a fatal error"
    );
    assert_eq!(
        fs::read_dir(ws.path("in")).unwrap().count(),
        6,
        "nothing moved"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fatal_error_stops_images_that_have_not_reached_the_api() {
    // Needs two images decoding at once: cat_0 is tiny and gets its 401 back
    // while cat_1, a large photo, is still being decoded.
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return;
    }
    let ws = Workspace::new();
    ws.jpeg("in/cat_0.jpg", 64, 64);
    ws.jpeg("in/cat_1.jpg", 6000, 4000);
    for i in 2..6 {
        ws.jpeg(&format!("in/cat_{i}.jpg"), 64, 64);
    }
    let mock = Mock::start(Arc::new(|_: &str, _| {
        Reply::error(401, "authentication_error", "invalid api key")
    }))
    .await;

    let output = ws.run(&mock, &["-j", "2"]).await;
    assert_eq!(
        mock.files_requested(),
        ["cat_0.jpg"],
        "no request may follow a rejected key\n{}",
        debug(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fatal_error_cancels_pending_retries() {
    let ws = Workspace::new();
    ws.jpeg("in/revoked.jpg", 64, 64);
    ws.jpeg("in/waiting.jpg", 64, 64);
    let mock = Mock::start(Arc::new(|file: &str, _| {
        if file.starts_with("waiting") {
            Reply {
                retry_after: Some("60"),
                ..Reply::error(503, "unavailable", "try again")
            }
        } else {
            Reply::error(401, "authentication_error", "invalid api key")
        }
    }))
    .await;

    // Without cancellation, waiting.jpg sits out a 60s Retry-After per retry.
    let output = tokio::time::timeout(Duration::from_secs(30), ws.run(&mock, &["-j", "2"]))
        .await
        .expect("the run must stop instead of waiting out the backoff");
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unwritable_report_path_fails_before_anything_moves() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    let mock = Mock::start(by_name()).await;

    let report = ws.str("missing-dir/report.json");
    let output = ws.run(&mock, &["--report", &report]).await;
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
    assert!(
        ws.exists("in/cat.jpg") && mock.requests().is_empty(),
        "nothing may happen when the report can't be written\n{}",
        debug(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sorting_in_place_recursively_skips_sorted_folders_and_dedupes() {
    let ws = Workspace::new();
    ws.jpeg("in/a/cat.jpg", 64, 64);
    ws.jpeg("in/b/cat.jpg", 64, 64);
    ws.jpeg("in/cats/already_sorted.jpg", 64, 64);
    ws.jpeg("in/unusable/shaky_earlier_run.jpg", 64, 64);
    ws.jpeg("in/.hidden/cat_secret.jpg", 64, 64);
    let mock = Mock::start(by_name()).await;

    let args: Vec<String> = [
        "-i",
        "in",
        "-o",
        "in",
        "-c",
        "categories.json",
        "--recursive",
    ]
    .iter()
    .map(|a| match *a {
        "in" => ws.str("in"),
        "categories.json" => ws.str("categories.json"),
        other => other.to_owned(),
    })
    .collect();
    let output = run_raw(&args, Some(&mock.url), Some("test-key"), &ws.path("config")).await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));

    assert_eq!(
        mock.files_requested(),
        ["cat.jpg", "cat.jpg"],
        "sorted, unusable and hidden folders are skipped"
    );
    assert!(ws.exists("in/cats/cat.jpg"));
    assert!(
        ws.exists("in/cats/cat (1).jpg"),
        "same-name files must not overwrite each other"
    );
    assert!(ws.exists("in/cats/already_sorted.jpg"));
    assert!(ws.exists("in/.hidden/cat_secret.jpg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_configuration_fails_before_any_request() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    let mock = Mock::start(by_name()).await;

    fs::write(ws.path("categories.json"), r#"{"only_one": null}"#).unwrap();
    let output = ws.run(&mock, &[]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("between 2 and 255"),
        "{}",
        debug(&output)
    );

    fs::write(ws.path("categories.json"), CATEGORIES).unwrap();
    let args: Vec<String> = [
        "-i",
        &ws.str("in"),
        "-o",
        &ws.str("out"),
        "-c",
        &ws.str("categories.json"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let output = run_raw(&args, Some(&mock.url), None, &ws.path("config")).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("SUBQ_API_KEY"),
        "without a terminal there is no prompt, just a clear error\n{}",
        debug(&output)
    );
    assert!(!ws.exists("config"), "nothing saved without a prompt");

    assert!(mock.requests().is_empty());
    assert!(ws.exists("in/cat.jpg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_the_saved_key_when_none_is_given() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    ws.save_key("test-key");
    let mock = Mock::start(by_name()).await;

    let output = ws.run_without_key(&mock).await;
    assert_eq!(output.status.code(), Some(0), "{}", debug(&output));
    assert_valid_request(&mock.requests()[0], 1280);
    assert!(ws.exists("out/cats/cat.jpg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_saved_key_is_forgotten() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    ws.save_key("sk-revoked");
    let mock = Mock::start(Arc::new(|_: &str, _| {
        Reply::error(401, "authentication_error", "Missing or invalid API key.")
    }))
    .await;

    let output = ws.run_without_key(&mock).await;
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
    assert!(
        stderr(&output).contains("Removed the rejected key"),
        "{}",
        debug(&output)
    );
    assert!(
        !ws.exists("config/credentials.json"),
        "next run should ask again"
    );
    assert!(ws.exists("in/cat.jpg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_missing_flags_are_an_error_not_a_prompt() {
    let ws = Workspace::new();
    ws.jpeg("in/cat.jpg", 64, 64);
    let mock = Mock::start(by_name()).await;

    let args = vec!["-i".to_owned(), ws.str("in")];
    let output = run_raw(&args, Some(&mock.url), Some("test-key"), &ws.path("config")).await;
    assert_eq!(output.status.code(), Some(1), "{}", debug(&output));
    let err = stderr(&output);
    assert!(err.contains("missing --output, --categories"), "{err}");
    assert!(mock.requests().is_empty());
    assert!(ws.exists("in/cat.jpg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sorts_camera_raws_by_their_embedded_previews() {
    let ws = Workspace::new();
    let cr3 = raw::Cr3 {
        thumb: Some(raw::jpeg(160, 120)),
        preview: Some(raw::jpeg(1620, 1080)),
        full: Some(raw::jpeg(1800, 1200)),
        // Shot in portrait: rotate 90° clockwise.
        orientation: Some(6),
    }
    .build();
    let files = [
        ("cat_IMG_0001.CR3", cr3.clone()),
        (
            "dog_IMG_0002.cr2",
            raw::cr2(&raw::jpeg(1440, 960), &raw::jpeg(160, 120), 1),
        ),
        ("cat_DSC_0003.NEF", raw::nef(&raw::jpeg(1440, 960), 1)),
        ("dog_DSCF0004.RAF", raw::raf(&raw::jpeg(1440, 960))),
        ("cat_P0005.RW2", raw::rw2(&raw::jpeg(1440, 960))),
    ];
    for (name, bytes) in &files {
        fs::write(ws.path(&format!("in/{name}")), bytes).unwrap();
    }
    fs::write(ws.path("in/broken.cr3"), &cr3[..cr3.len() / 2]).unwrap();

    let mock = Mock::start(by_name()).await;
    let output = ws.run(&mock, &[]).await;
    assert_eq!(output.status.code(), Some(3), "{}", debug(&output));

    for (name, bytes) in &files {
        let folder = if name.starts_with("cat") {
            "cats"
        } else {
            "dogs"
        };
        assert_eq!(
            &ws.read(&format!("out/{folder}/{name}")),
            bytes,
            "the original raw is moved untouched"
        );
    }
    assert!(ws.exists("in/broken.cr3"), "failed files stay in place");
    let err = stderr(&output);
    assert!(
        err.contains("broken.cr3") && err.contains("`mdat` box is truncated"),
        "{err}"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), files.len());
    for request in &requests {
        let name = file_name_of(&request.body);
        let preview = assert_valid_request(request, 1280);
        let expected = if name.ends_with(".CR3") {
            (853, 1280)
        } else {
            (1280, 853)
        };
        assert_eq!(preview.dimensions(), expected, "{name}");
    }
}
