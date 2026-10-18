//! Client for the SubQ Decisions API (`POST /v1/systemone`).

use std::{
    collections::{hash_map::RandomState, HashMap},
    error::Error as StdError,
    fmt,
    hash::BuildHasher,
    time::Duration,
};

use reqwest::{
    header::{HeaderMap, CONTENT_TYPE, RETRY_AFTER},
    StatusCode,
};
use serde::{ser::SerializeMap, Deserialize, Serialize, Serializer};
use serde_json::Value;
use tokio::sync::watch;

use crate::categories::Categories;

pub const DEFAULT_API_URL: &str = "https://api.subq.ai/v1/systemone";
pub const DEFAULT_MODEL: &str = "subq-snap-preview";

const QUESTION_KEY: &str = "target_folder";
const INSTRUCTIONS: &str = "Select the single best folder category for this image.";

const UNUSABLE_KEY: &str = "unusable";
const UNUSABLE_INSTRUCTIONS: &str = "Is this photo unusable?";
const UNUSABLE_YES: &str =
    "Unusable: badly blurred, out of focus or smeared by motion, far too dark \
     or too bright to make out the subject, or an accidental shot (the ground, a pocket, a finger \
     or something else blocking the lens)";
const UNUSABLE_NO: &str = "Usable: the subject can be seen clearly, even if the photo is not \
     perfect (slight softness, noise or plain composition are fine)";

const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(120);

pub struct ClientConfig {
    pub url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub pool_size: usize,
}

pub struct SubqClient {
    http: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_retries: u32,
}

/// The model's verdict for one image.
#[derive(Debug, Clone)]
pub struct Decision {
    pub choice: String,
    pub confidence: f64,
    /// Per-category probabilities, highest first.
    pub probabilities: Vec<(String, f64)>,
    /// Probability (0-1) that the photo is unusable; `None` unless asked.
    pub unusable: Option<f64>,
    pub input_tokens: u64,
}

#[derive(Debug)]
pub enum ApiError {
    /// Credentials, billing, permissions or endpoint are wrong: every
    /// remaining request would fail the same way, so the run should stop.
    Fatal { status: u16, message: String },
    /// The API rejected this particular request (e.g. an invalid image).
    Rejected { status: u16, message: String },
    /// Rate limits, server errors or network failures outlasted the retries.
    RetriesExhausted { attempts: u32, last_error: String },
    /// A 2xx response that does not match the documented schema.
    InvalidResponse(String),
    /// The run stopped while this request was waiting to be retried.
    Cancelled,
}

impl ApiError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, ApiError::Fatal { .. })
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Fatal { status, .. } | ApiError::Rejected { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Fatal { status, message } => {
                let hint = match status {
                    402 => " (add credits to your SubQ account)",
                    403 => " (this API key cannot use the requested model)",
                    404 | 405 => " (check --api-url)",
                    _ => "",
                };
                write!(
                    f,
                    "SubQ API returned {}: {message}{hint}",
                    status_text(*status)
                )
            }
            ApiError::Rejected { status, message } => {
                write!(
                    f,
                    "SubQ API rejected the image ({}): {message}",
                    status_text(*status)
                )
            }
            ApiError::RetriesExhausted {
                attempts,
                last_error,
            } => {
                write!(f, "gave up after {attempts} attempts: {last_error}")
            }
            ApiError::InvalidResponse(message) => {
                write!(f, "unexpected response from SubQ API: {message}")
            }
            ApiError::Cancelled => f.write_str("stopped before the request could be retried"),
        }
    }
}

impl StdError for ApiError {}

impl SubqClient {
    pub fn new(config: ClientConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("snapsort/", env!("CARGO_PKG_VERSION")))
            .timeout(config.timeout)
            .connect_timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(config.pool_size)
            .build()?;
        Ok(Self {
            http,
            url: config.url,
            api_key: config.api_key,
            model: config.model,
            max_retries: config.max_retries,
        })
    }

    /// Ask SubQ which category `image_base64` (a JPEG) belongs to and, with
    /// `check_unusable`, how likely the photo is to be unusable (same
    /// request). Transient failures (429, 5xx, timeouts, connection errors)
    /// are retried with jittered exponential backoff, honouring
    /// `Retry-After`, unless `stop` turns true first: then it returns
    /// [`ApiError::Cancelled`].
    pub async fn classify(
        &self,
        file_name: &str,
        image_base64: &str,
        categories: &Categories,
        check_unusable: bool,
        mut stop: watch::Receiver<bool>,
    ) -> Result<Decision, ApiError> {
        let payload = DecisionRequest {
            model: &self.model,
            state: format!("Classify file: {file_name}"),
            images: [ImageInput {
                media_type: "image/jpeg",
                base64: image_base64,
            }],
            questions: Questions::new(categories, check_unusable),
        };
        let body = serde_json::to_vec(&payload).expect("request payload is always serializable");
        // A buffered body makes the builder cheaply cloneable for retries.
        let request = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        let mut attempt = 0;
        loop {
            attempt += 1;
            let req = request
                .try_clone()
                .expect("buffered request body is cloneable");
            let (last_error, retry_after) = match req.send().await {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    match response.text().await {
                        Ok(text) if status.is_success() => {
                            return parse_decision(&text, check_unusable)
                        }
                        Ok(text) => {
                            let message = error_message(&text);
                            match classify_status(status) {
                                StatusClass::Fatal => {
                                    return Err(ApiError::Fatal {
                                        status: status.as_u16(),
                                        message,
                                    })
                                }
                                StatusClass::Reject => {
                                    return Err(ApiError::Rejected {
                                        status: status.as_u16(),
                                        message,
                                    })
                                }
                                StatusClass::Retry => (
                                    format!("{}: {message}", status_text(status.as_u16())),
                                    retry_after,
                                ),
                            }
                        }
                        Err(err) => (describe_error(&err), retry_after),
                    }
                }
                Err(err) => (describe_error(&err), None),
            };

            if attempt > self.max_retries {
                return Err(ApiError::RetriesExhausted {
                    attempts: attempt,
                    last_error,
                });
            }
            // A backoff can last minutes; don't sit it out once the run has
            // stopped (Ctrl-C, or a fatal error on another image).
            tokio::select! {
                () = tokio::time::sleep(retry_delay(attempt, retry_after)) => {}
                Ok(_) = stop.wait_for(|stop| *stop) => return Err(ApiError::Cancelled),
            }
        }
    }
}

// ----- Request schema -------------------------------------------------------

#[derive(Serialize)]
struct DecisionRequest<'a> {
    model: &'a str,
    state: String,
    images: [ImageInput<'a>; 1],
    questions: Questions<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageInput<'a> {
    media_type: &'static str,
    base64: &'a str,
}

#[derive(Serialize)]
struct Questions<'a> {
    target_folder: ChoiceQuestion<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unusable: Option<NoulQuestion>,
}

impl<'a> Questions<'a> {
    fn new(categories: &'a Categories, check_unusable: bool) -> Self {
        Self {
            target_folder: ChoiceQuestion {
                kind: "choice",
                instructions: INSTRUCTIONS,
                criteria: Criteria(categories),
            },
            unusable: check_unusable.then_some(NoulQuestion {
                kind: "noul",
                instructions: UNUSABLE_INSTRUCTIONS,
                criteria: NoulCriteria {
                    yes: UNUSABLE_YES,
                    no: UNUSABLE_NO,
                },
            }),
        }
    }
}

#[derive(Serialize)]
struct ChoiceQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'static str,
    criteria: Criteria<'a>,
}

/// A yes/no question; SubQ answers with the probability of yes.
#[derive(Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'static str,
    criteria: NoulCriteria,
}

#[derive(Serialize)]
struct NoulCriteria {
    #[serde(rename = "true")]
    yes: &'static str,
    #[serde(rename = "false")]
    no: &'static str,
}

/// Serializes as `{ "<folder>": "<description>" | null, ... }` in file order.
struct Criteria<'a>(&'a Categories);

impl Serialize for Criteria<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for category in self.0.iter() {
            map.serialize_entry(&category.name, &category.description)?;
        }
        map.end()
    }
}

// ----- Response schema ------------------------------------------------------

#[derive(Deserialize)]
struct DecisionResponse {
    answers: HashMap<String, Value>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
}

#[derive(Deserialize)]
struct ChoiceAnswer {
    choice: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct NoulAnswer {
    noul: f64,
}

/// `check_unusable`: the request asked the unusable question, so its answer
/// must be there too.
fn parse_decision(body: &str, check_unusable: bool) -> Result<Decision, ApiError> {
    let mut response: DecisionResponse = serde_json::from_str(body)
        .map_err(|err| ApiError::InvalidResponse(format!("{err} in {}", truncate(body, 200))))?;
    let answer = response
        .answers
        .remove(QUESTION_KEY)
        .ok_or_else(|| ApiError::InvalidResponse(format!("no `{QUESTION_KEY}` answer")))?;
    let answer: ChoiceAnswer = serde_json::from_value(answer).map_err(|err| {
        ApiError::InvalidResponse(format!("malformed `{QUESTION_KEY}` answer: {err}"))
    })?;

    // Prefer the model's calibrated confidence; fall back to the winning
    // probability. A missing or non-finite value counts as zero so the image
    // is routed to review rather than trusted blindly.
    let confidence = answer
        .confidence
        .or_else(|| answer.probabilities.get(&answer.choice).copied())
        .filter(|c| c.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);

    let mut probabilities: Vec<(String, f64)> = answer.probabilities.into_iter().collect();
    probabilities.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let unusable = if check_unusable {
        let answer = response
            .answers
            .remove(UNUSABLE_KEY)
            .ok_or_else(|| ApiError::InvalidResponse(format!("no `{UNUSABLE_KEY}` answer")))?;
        let answer: NoulAnswer = serde_json::from_value(answer).map_err(|err| {
            ApiError::InvalidResponse(format!("malformed `{UNUSABLE_KEY}` answer: {err}"))
        })?;
        Some(answer.noul.clamp(0.0, 1.0))
    } else {
        None
    };

    Ok(Decision {
        choice: answer.choice,
        confidence,
        probabilities,
        unusable,
        input_tokens: response.usage.map_or(0, |u| u.input_tokens),
    })
}

// ----- Error handling and retries -------------------------------------------

enum StatusClass {
    Fatal,
    Reject,
    Retry,
}

fn classify_status(status: StatusCode) -> StatusClass {
    match status.as_u16() {
        401..=405 => StatusClass::Fatal,
        408 | 409 | 425 | 429 | 500..=599 => StatusClass::Retry,
        _ => StatusClass::Reject,
    }
}

/// Pull a human-readable message out of the error formats we may see:
/// SubQ (`{"detail": {"error_type", "message"}}`), FastAPI validation errors
/// (`{"detail": [{"msg": ...}]}`), and OpenAI/OpenRouter style
/// (`{"error": {"message": ...}}`).
fn error_message(body: &str) -> String {
    let fallback = || {
        let text = body.trim();
        if text.is_empty() {
            "no error details".to_owned()
        } else {
            truncate(text, 300)
        }
    };
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return fallback();
    };
    let detail = &json["detail"];
    let message = match detail {
        Value::String(s) => Some(s.clone()),
        Value::Object(obj) => {
            let msg = obj.get("message").and_then(Value::as_str);
            let kind = obj.get("error_type").and_then(Value::as_str);
            match (kind, msg) {
                (Some(kind), Some(msg)) => Some(format!("{kind}: {msg}")),
                (None, Some(msg)) => Some(msg.to_owned()),
                _ => None,
            }
        }
        Value::Array(items) => {
            let msgs: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    let msg = item.get("msg").or_else(|| item.get("message"))?.as_str()?;
                    let loc = item.get("loc").and_then(Value::as_array).map(|loc| {
                        loc.iter()
                            .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_owned))
                            .collect::<Vec<_>>()
                            .join(".")
                    });
                    Some(match loc {
                        Some(loc) if !loc.is_empty() => format!("{loc}: {msg}"),
                        _ => msg.to_owned(),
                    })
                })
                .collect();
            (!msgs.is_empty()).then(|| msgs.join("; "))
        }
        _ => None,
    };
    message
        .or_else(|| json["error"]["message"].as_str().map(str::to_owned))
        .or_else(|| json["message"].as_str().map(str::to_owned))
        .unwrap_or_else(fallback)
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::from_secs_f64(seconds).min(RETRY_AFTER_MAX))
}

fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay;
    }
    let exp = BACKOFF_BASE
        .saturating_mul(1 << (attempt - 1).min(16))
        .min(BACKOFF_MAX);
    // "Equal jitter": half fixed, half random, so concurrent workers that hit
    // the same rate limit spread out instead of retrying in lockstep.
    let half = exp / 2;
    half + half.mul_f64(random_unit())
}

/// A random number in [0, 1) without pulling in a RNG crate.
fn random_unit() -> f64 {
    let bits = RandomState::new().hash_one(std::time::Instant::now());
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

fn describe_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "request timed out".into();
    }
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn status_text(status: u16) -> String {
    match StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
    {
        Some(reason) => format!("{status} {reason}"),
        None => status.to_string(),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &text[..idx]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_matches_documented_schema() {
        let categories = Categories::parse(r#"{"cats": "Feline friends", "dogs": null}"#).unwrap();
        let payload = DecisionRequest {
            model: DEFAULT_MODEL,
            state: "Classify file: a.jpg".into(),
            images: [ImageInput {
                media_type: "image/jpeg",
                base64: "AAAA",
            }],
            questions: Questions::new(&categories, false),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "model": "subq-snap-preview",
                "state": "Classify file: a.jpg",
                "images": [{ "mediaType": "image/jpeg", "base64": "AAAA" }],
                "questions": {
                    "target_folder": {
                        "type": "choice",
                        "instructions": "Select the single best folder category for this image.",
                        "criteria": { "cats": "Feline friends", "dogs": null }
                    }
                }
            })
        );
    }

    #[test]
    fn unusable_question_is_a_noul_next_to_the_folder_question() {
        let categories = Categories::parse(r#"{"cats": null, "dogs": null}"#).unwrap();
        let json = serde_json::to_value(Questions::new(&categories, true)).unwrap();
        assert_eq!(json["target_folder"]["type"], "choice");
        let unusable = &json["unusable"];
        assert_eq!(unusable["type"], "noul");
        assert_eq!(unusable["instructions"], "Is this photo unusable?");
        assert!(unusable["criteria"]["true"]
            .as_str()
            .unwrap()
            .starts_with("Unusable: badly blurred"));
        assert!(unusable["criteria"]["false"]
            .as_str()
            .unwrap()
            .starts_with("Usable: "));
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn parses_choice_answer() {
        let body = r#"{
            "model": "subq-snap-preview",
            "answers": { "target_folder": {
                "type": "choice", "choice": "dogs", "confidence": 0.81,
                "probabilities": { "cats": 0.1, "dogs": 0.9 }
            }},
            "usage": { "input_tokens": 312, "output_tokens": 0 }
        }"#;
        let decision = parse_decision(body, false).unwrap();
        assert_eq!(decision.choice, "dogs");
        assert_eq!(decision.confidence, 0.81);
        assert_eq!(decision.probabilities[0], ("dogs".into(), 0.9));
        assert_eq!(decision.unusable, None);
        assert_eq!(decision.input_tokens, 312);
    }

    #[test]
    fn parses_unusable_answer_only_when_asked() {
        let body = r#"{"answers":{
            "target_folder":{"choice":"a","confidence":0.9},
            "unusable":{"type":"noul","noul":0.92}
        }}"#;
        assert_eq!(parse_decision(body, true).unwrap().unusable, Some(0.92));
        assert_eq!(parse_decision(body, false).unwrap().unusable, None);

        let missing = r#"{"answers":{"target_folder":{"choice":"a","confidence":0.9}}}"#;
        assert!(parse_decision(missing, true).is_err());
        let malformed = r#"{"answers":{"target_folder":{"choice":"a"},"unusable":{"noul":"yes"}}}"#;
        assert!(parse_decision(malformed, true).is_err());
    }

    #[test]
    fn confidence_falls_back_to_probability_then_zero() {
        let with_probs =
            r#"{"answers":{"target_folder":{"choice":"a","probabilities":{"a":0.7,"b":0.3}}}}"#;
        assert_eq!(parse_decision(with_probs, false).unwrap().confidence, 0.7);
        let bare = r#"{"answers":{"target_folder":{"choice":"a"}}}"#;
        assert_eq!(parse_decision(bare, false).unwrap().confidence, 0.0);
    }

    #[test]
    fn rejects_responses_without_our_answer() {
        assert!(parse_decision(r#"{"answers":{}}"#, false).is_err());
        assert!(parse_decision("not json", false).is_err());
    }

    #[test]
    fn extracts_error_messages() {
        assert_eq!(
            error_message(r#"{"detail":{"error_type":"validation_error","message":"bad image"}}"#),
            "validation_error: bad image"
        );
        assert_eq!(
            error_message(r#"{"detail":[{"loc":["body","images",0],"msg":"too large"}]}"#),
            "body.images.0: too large"
        );
        assert_eq!(
            error_message(r#"{"error":{"code":401,"message":"nope"}}"#),
            "nope"
        );
        assert_eq!(error_message("Bad Gateway"), "Bad Gateway");
        assert_eq!(error_message(""), "no error details");
    }

    #[test]
    fn backoff_is_bounded_and_honours_retry_after() {
        for attempt in 1..30 {
            let delay = retry_delay(attempt, None);
            assert!(delay <= BACKOFF_MAX, "attempt {attempt}: {delay:?}");
        }
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn status_classes() {
        assert!(matches!(
            classify_status(StatusCode::UNAUTHORIZED),
            StatusClass::Fatal
        ));
        assert!(matches!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            StatusClass::Retry
        ));
        assert!(matches!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            StatusClass::Retry
        ));
        assert!(matches!(
            classify_status(StatusCode::UNPROCESSABLE_ENTITY),
            StatusClass::Reject
        ));
    }
}
