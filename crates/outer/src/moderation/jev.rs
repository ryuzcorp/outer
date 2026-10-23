//! Jev classification through Cloudflare AI Gateway (spec §15.1).
//!
//! One [`JevClient::classify`] call fans a deploy's new chunk texts out as a
//! state object (`c0`, `c1`, …) with five questions per chunk, batched at
//! ~2k tokens of state and up to four batches in flight. Any failure after
//! the one retry (empty balance, gateway auth, 5xx, timeout, malformed
//! answer) is [`JevError::Unavailable`]: the gate fails closed and nothing
//! is cached or published. Secrets never appear in errors, logs or `Debug`.

use std::time::Duration;

use futures::stream::{self, StreamExt};

/// The Jev model served through AI Gateway.
pub const JEV_MODEL: &str = "typesafe/jev-1.14.0";
/// `https://api.cloudflare.com/client/v4`, overridable for tests.
const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// Per-attempt request timeout in production (spec §15.1: 3 s + one retry).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
/// Tokens of chunk text per classify call, estimated as chars/4.
const MAX_STATE_TOKENS_PER_CALL: usize = 2000;
/// Classify calls in flight at once.
const MAX_PARALLEL_CALLS: usize = 4;

/// What the gate needs to reach Jev. Built once at boot from the config's
/// Cloudflare pair plus `OUTER_AI_GATEWAY`; tests point `api_base` at a
/// fake server on 127.0.0.1.
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub api_base: String,
    pub account_id: String,
    pub api_token: String,
    pub gateway: String,
    pub timeout: Duration,
}

impl JevConfig {
    pub fn new(account_id: String, api_token: String, gateway: String) -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            account_id,
            api_token,
            gateway,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Jev's scores for one chunk of text.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChunkScores {
    /// Probability of strong profanity, obscene language or slurs.
    pub profanity: f32,
    /// Probability of sexual content involving minors.
    pub minors: f32,
    /// Probability of phishing or credential harvesting.
    pub phishing: f32,
    /// Overall severity, 0 (none) to 3 (severe).
    pub severity: f32,
    /// Probability of hate speech or harassment.
    pub hate: f32,
}

/// Jev couldn't classify: the caller fails closed. The message names the
/// cause (status, envelope code, timeout) and never carries secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JevError {
    #[error("jev unavailable: {0}")]
    Unavailable(String),
}

/// Classifies chunk texts through AI Gateway. `Clone` shares the underlying
/// connection pool.
#[derive(Clone)]
pub struct JevClient {
    config: JevConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for JevClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevClient")
            .field("api_base", &self.config.api_base)
            .field("account_id", &self.config.account_id)
            .field("api_token", &"<redacted>")
            .field("gateway", &self.config.gateway)
            .field("timeout", &self.config.timeout)
            .finish()
    }
}

impl JevClient {
    pub fn new(config: JevConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| anyhow::anyhow!("building the Jev client: {e:#}"))?;
        Ok(Self { config, client })
    }

    /// Classify every text, preserving order. Empty input makes no calls.
    /// Batches hold ~2k tokens of state; up to four batches run at once.
    pub async fn classify(&self, texts: &[String]) -> Result<Vec<ChunkScores>, JevError> {
        let batches = pack_batches(texts);
        let out = stream::iter(batches)
            .map(|batch| self.classify_batch(texts, batch))
            .buffered(MAX_PARALLEL_CALLS)
            .collect::<Vec<_>>()
            .await;
        let mut scores = vec![ChunkScores::default(); texts.len()];
        for batch_out in out {
            let (batch, batch_scores) = batch_out?;
            for (i, s) in batch.iter().zip(batch_scores) {
                scores[*i] = s;
            }
        }
        Ok(scores)
    }

    /// One state object plus its questions, with one retry on failure.
    async fn classify_batch(
        &self,
        texts: &[String],
        batch: Vec<usize>,
    ) -> Result<(Vec<usize>, Vec<ChunkScores>), JevError> {
        let url = format!(
            "{}/accounts/{}/ai/run",
            self.config.api_base.trim_end_matches('/'),
            self.config.account_id
        );
        let mut state = serde_json::Map::with_capacity(batch.len());
        let mut questions = serde_json::Map::with_capacity(batch.len() * 5);
        for (slot, chunk) in batch.iter().enumerate() {
            let key = format!("c{slot}");
            state.insert(
                key.clone(),
                serde_json::Value::String(texts[*chunk].clone()),
            );
            for (qid, question) in questions_for(&key) {
                questions.insert(qid, question);
            }
        }
        let body = serde_json::json!({
            "model": JEV_MODEL,
            "input": { "state": state, "questions": questions },
        });
        let mut attempt = self.post(&url, &body).await;
        if attempt.is_err() {
            attempt = self.post(&url, &body).await;
        }
        let answers = attempt?;
        let mut scores = Vec::with_capacity(batch.len());
        for slot in 0..batch.len() {
            let key = format!("c{slot}");
            scores.push(ChunkScores {
                profanity: noul(&answers, &format!("{key}_profanity"))?,
                minors: noul(&answers, &format!("{key}_minors"))?,
                phishing: noul(&answers, &format!("{key}_phishing"))?,
                severity: score(&answers, &format!("{key}_severity"))?,
                hate: noul(&answers, &format!("{key}_hate"))?,
            });
        }
        Ok((batch, scores))
    }

    /// One POST; any transport, status or envelope problem is `Unavailable`.
    async fn post(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Map<String, serde_json::Value>, JevError> {
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.config.api_token)
            .header(
                "cf-aig-authorization",
                format!("Bearer {}", self.config.api_token),
            )
            .header("cf-aig-gateway-id", &self.config.gateway)
            .json(body)
            .send()
            .await
            .map_err(|e| JevError::Unavailable(scrubbed(&e)))?;
        if !response.status().is_success() {
            let status = response.status();
            let snippet = bounded_text(response).await;
            return Err(JevError::Unavailable(format!(
                "jev http {status}: {snippet}"
            )));
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| JevError::Unavailable(format!("jev answered non-JSON: {e:#}")))?;
        if value.get("success").and_then(|s| s.as_bool()) == Some(false) {
            return Err(JevError::Unavailable(format!(
                "jev rejected the batch: {}",
                envelope_errors(&value)
            )));
        }
        // AI Gateway wraps the run twice: `result.result` holds the model
        // payload, with a `state` that must read `Completed`. Also accept
        // `result.answers` and bare `answers` for tolerance.
        let outer = match value.get("result") {
            Some(result) if result.is_object() => result,
            _ => &value,
        };
        if let Some(state) = outer.get("state").and_then(|s| s.as_str()) {
            if state != "Completed" {
                return Err(JevError::Unavailable(format!("jev run state is {state}")));
            }
        }
        let payload = match outer.get("result") {
            Some(inner) if inner.is_object() => inner,
            _ => outer,
        };
        payload
            .get("answers")
            .and_then(|a| a.as_object())
            .cloned()
            .ok_or_else(|| JevError::Unavailable("jev response has no answers object".to_string()))
    }
}

/// Greedy packs of chunk indices, each ~2k tokens of state. Oversized
/// chunks travel alone rather than blocking the batcher.
fn pack_batches(texts: &[String]) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut tokens = 0;
    let mut current: Vec<usize> = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        let estimate = text.chars().count() / 4;
        if !current.is_empty() && tokens + estimate > MAX_STATE_TOKENS_PER_CALL {
            batches.push(std::mem::take(&mut current));
            tokens = 0;
        }
        current.push(i);
        tokens += estimate;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// The five questions for one state field. Instructions stay in English
/// (Jev is strongest there) and reference the field in backticks.
fn questions_for(key: &str) -> Vec<(String, serde_json::Value)> {
    let noul =
        |instructions: &str| serde_json::json!({ "type": "noul", "instructions": instructions });
    vec![
        (
            format!("{key}_profanity"),
            noul(&format!(
                "Does `{key}` contain strong profanity, obscene language or slurs? \
                 Mild words like damn, hell or crap don't count. \
                 Masked or creative spellings do."
            )),
        ),
        (
            format!("{key}_minors"),
            noul(&format!(
                "Does `{key}` contain sexual content involving minors?"
            )),
        ),
        (
            format!("{key}_phishing"),
            noul(&format!(
                "Does `{key}` contain phishing, fraud, or attempts to harvest \
                 credentials such as passwords?"
            )),
        ),
        (
            format!("{key}_severity"),
            serde_json::json!({
                "type": "score",
                "instructions": format!(
                    "Rate the overall severity of harm in `{key}`: \
                     0 for none, 1 for mild, 2 for serious, 3 for severe."
                ),
                "criteria": [
                    "no harmful content",
                    "mildly offensive or crude content",
                    "seriously harmful content such as threats or explicit hate",
                    "severely harmful content such as sexual content involving minors \
                     or instructions facilitating violence",
                ],
            }),
        ),
        (
            format!("{key}_hate"),
            noul(&format!(
                "Does `{key}` contain hate speech or harassment targeting a person or group?"
            )),
        ),
    ]
}

/// A Noul probability answer; a missing or misshapen answer fails closed.
fn noul(answers: &serde_json::Map<String, serde_json::Value>, qid: &str) -> Result<f32, JevError> {
    answers
        .get(qid)
        .and_then(|a| a.get("noul"))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .ok_or_else(|| JevError::Unavailable(format!("jev answer {qid} is missing or not a noul")))
}

/// A score answer; a missing or misshapen answer fails closed.
fn score(answers: &serde_json::Map<String, serde_json::Value>, qid: &str) -> Result<f32, JevError> {
    answers
        .get(qid)
        .and_then(|a| a.get("score"))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .ok_or_else(|| JevError::Unavailable(format!("jev answer {qid} is missing or not a score")))
}

/// `code 2021: message; …` from a Cloudflare envelope.
fn envelope_errors(value: &serde_json::Value) -> String {
    let Some(errors) = value.get("errors").and_then(|e| e.as_array()) else {
        return "unknown error".to_string();
    };
    let parts: Vec<String> = errors
        .iter()
        .map(|e| match (e.get("code"), e.get("message")) {
            (Some(code), Some(message)) => format!("code {code}: {message}"),
            _ => e.to_string(),
        })
        .collect();
    const CAP: usize = 500;
    let joined = parts.join("; ");
    if joined.len() <= CAP {
        joined
    } else {
        let mut end = CAP;
        while !joined.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &joined[..end])
    }
}

/// A transport failure without secrets: reqwest errors carry the URL (which
/// holds only the account id) but never headers or the token.
fn scrubbed(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "request timed out".to_string();
    }
    if err.is_connect() {
        return "could not connect".to_string();
    }
    format!("request failed: {err:#}")
}

/// The first bytes of an error body, for logs only.
async fn bounded_text(response: reqwest::Response) -> String {
    const CAP: usize = 300;
    let bytes = response.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim();
    if trimmed.len() <= CAP {
        trimmed.to_string()
    } else {
        let mut end = CAP;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A hand-rolled Jev stand-in on 127.0.0.1: records requests (headers,
    //! body), answers from a script, tracks in-flight calls. No framework,
    //! so timeouts and concurrency stay deterministic.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    pub(crate) struct RecordedRequest {
        pub path: String,
        pub headers: HashMap<String, String>,
        pub body: serde_json::Value,
    }

    pub(crate) type Responder =
        Arc<dyn Fn(&serde_json::Value) -> (u16, serde_json::Value) + Send + Sync>;

    pub(crate) struct FakeJev {
        pub base_url: String,
        pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
        pub max_in_flight: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeJev {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl FakeJev {
        pub(crate) async fn start(responder: Responder) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let in_flight = Arc::new(AtomicUsize::new(0));
            let max_in_flight = Arc::new(AtomicUsize::new(0));
            let task = tokio::spawn(serve(
                listener,
                responder,
                requests.clone(),
                in_flight.clone(),
                max_in_flight.clone(),
            ));
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                requests,
                max_in_flight,
                task,
            }
        }

        pub(crate) fn calls(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    async fn serve(
        listener: tokio::net::TcpListener,
        responder: Responder,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    ) {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let responder = responder.clone();
            let requests = requests.clone();
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            tokio::spawn(async move {
                handle(conn, &responder, &requests, &in_flight, &max_in_flight).await;
            });
        }
    }

    async fn handle(
        conn: tokio::net::TcpStream,
        responder: &Responder,
        requests: &Arc<Mutex<Vec<RecordedRequest>>>,
        in_flight: &Arc<AtomicUsize>,
        max_in_flight: &Arc<AtomicUsize>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(conn);
        let mut request_line = String::new();
        if lines.read_line(&mut request_line).await.is_err() {
            return;
        }
        let mut headers = HashMap::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.is_err() {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && lines.read_exact(&mut body).await.is_err() {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        requests.lock().unwrap().push(RecordedRequest {
            path,
            headers,
            body: parsed.clone(),
        });
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max_in_flight.fetch_max(now, Ordering::SeqCst);
        // Responders may sleep to simulate latency; blocking the runtime
        // thread would serialize every batch, so run them on the pool.
        let responder = (*responder).clone();
        let (status, answer) = tokio::task::spawn_blocking(move || responder(&parsed))
            .await
            .unwrap_or_else(|_| (500, serde_json::json!({"error": "responder panicked"})));
        in_flight.fetch_sub(1, Ordering::SeqCst);
        let payload = serde_json::to_string(&answer).unwrap_or_default();
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            500 => "Internal Server Error",
            _ => "Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut conn = lines.into_inner();
        let _ = conn.write_all(response.as_bytes()).await;
    }

    /// An all-clear answer set for whatever questions the batch asked.
    pub(crate) fn clear_answers(body: &serde_json::Value) -> serde_json::Value {
        let mut answers = serde_json::Map::new();
        if let Some(questions) = body.pointer("/input/questions").and_then(|q| q.as_object()) {
            for (qid, question) in questions {
                let kind = question.get("type").and_then(|t| t.as_str()).unwrap_or("");
                answers.insert(
                    qid.clone(),
                    if kind == "score" {
                        serde_json::json!({"type": "score", "score": 0.0, "confidence": 0.9})
                    } else {
                        serde_json::json!({"type": "noul", "noul": 0.01})
                    },
                );
            }
        }
        serde_json::json!({
            "answers": answers,
            "usage": {},
        })
    }

    pub(crate) fn envelope(answers: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "errors": [],
            "result": answers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn test_client(base_url: &str) -> JevClient {
        let mut config = JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = base_url.to_string();
        config.timeout = Duration::from_millis(500);
        JevClient::new(config).unwrap()
    }

    fn answer_for(question: &serde_json::Value, noul: f64, score: f64) -> serde_json::Value {
        if question.get("type").and_then(|t| t.as_str()) == Some("score") {
            serde_json::json!({"type": "score", "score": score})
        } else {
            serde_json::json!({"type": "noul", "noul": noul})
        }
    }

    #[tokio::test]
    async fn request_shape_has_gateway_headers_model_state_and_questions() {
        let fake = FakeJev::start(Arc::new(|body| {
            let mut answers = serde_json::Map::new();
            let questions = body
                .pointer("/input/questions")
                .unwrap()
                .as_object()
                .unwrap();
            for (qid, question) in questions {
                answers.insert(qid.clone(), answer_for(question, 0.01, 0.0));
            }
            (200, envelope(serde_json::json!({"answers": answers})))
        }))
        .await;
        let client = test_client(&fake.base_url);
        let texts = vec!["hello world".to_string(), "second chunk".to_string()];
        client.classify(&texts).await.unwrap();

        assert_eq!(fake.calls(), 1);
        let recorded = fake.requests.lock().unwrap();
        let req = &recorded[0];
        assert_eq!(req.path, "/accounts/test-account/ai/run");
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("Bearer test-token")
        );
        assert_eq!(
            req.headers.get("cf-aig-authorization").map(String::as_str),
            Some("Bearer test-token")
        );
        assert_eq!(
            req.headers.get("cf-aig-gateway-id").map(String::as_str),
            Some("outer")
        );
        assert_eq!(req.body["model"], serde_json::json!(JEV_MODEL));
        assert_eq!(
            req.body["input"]["state"],
            serde_json::json!({"c0": "hello world", "c1": "second chunk"})
        );
        let questions = req.body["input"]["questions"].as_object().unwrap();
        for (slot, key) in ["c0", "c1"].iter().enumerate() {
            for suffix in ["profanity", "minors", "phishing", "severity", "hate"] {
                let qid = format!("{key}_{suffix}");
                let question = &questions[&qid];
                let expected = if suffix == "severity" {
                    "score"
                } else {
                    "noul"
                };
                assert_eq!(question["type"], serde_json::json!(expected), "{qid}");
                let instructions = question["instructions"].as_str().unwrap();
                assert!(
                    instructions.contains(&format!("`{key}`")),
                    "{qid} references its state field: {instructions}"
                );
            }
            let _ = slot;
        }
        let severity = &questions["c0_severity"];
        assert_eq!(severity["criteria"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn bare_response_parses_like_the_envelope() {
        let responder: Responder = Arc::new(|body| {
            let mut answers = serde_json::Map::new();
            let questions = body
                .pointer("/input/questions")
                .unwrap()
                .as_object()
                .unwrap();
            for (qid, question) in questions {
                let value = if qid.ends_with("_profanity") {
                    answer_for(question, 0.87, 0.0)
                } else if qid.ends_with("_severity") {
                    answer_for(question, 0.0, 2.6)
                } else {
                    answer_for(question, 0.02, 0.0)
                };
                answers.insert(qid.clone(), value);
            }
            (
                200,
                serde_json::json!({"model": "jev-1.14.0", "answers": answers}),
            )
        });
        let fake = FakeJev::start(responder).await;
        let scores = test_client(&fake.base_url)
            .classify(&["some text".to_string()])
            .await
            .unwrap();
        assert_eq!(scores.len(), 1);
        assert!((scores[0].profanity - 0.87).abs() < 1e-6);
        assert!((scores[0].severity - 2.6).abs() < 1e-6);
        assert!((scores[0].minors - 0.02).abs() < 1e-6);
    }
    #[tokio::test]
    async fn gateway_envelope_parses_doubly_nested_result() {
        // The exact shape AI Gateway returns for /ai/run (observed
        // 2026-10-23): the model payload sits at `result.result`, with a
        // run `state` beside it.
        let fake = FakeJev::start(Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "success": true,
                    "errors": [],
                    "messages": [],
                    "result": {
                        "state": "Completed",
                        "result": {
                            "model": "jev-1.14.0",
                            "answers": {
                                "c0_profanity": {"type": "noul", "noul": 0.95},
                                "c0_minors": {"type": "noul", "noul": 0.01},
                                "c0_phishing": {"type": "noul", "noul": 0.02},
                                "c0_severity": {"type": "score", "score": 1.43, "confidence": 0.35, "probabilities": {"0": 0.1}},
                                "c0_hate": {"type": "noul", "noul": 0.03},
                            },
                            "usage": {},
                        },
                    },
                }),
            )
        }))
        .await;
        let scores = test_client(&fake.base_url)
            .classify(&["anything".to_string()])
            .await
            .unwrap();
        assert_eq!(scores.len(), 1);
        assert!((scores[0].profanity - 0.95).abs() < 1e-6);
        assert!((scores[0].severity - 1.43).abs() < 1e-6);
        assert!((scores[0].phishing - 0.02).abs() < 1e-6);
    }

    #[tokio::test]
    async fn non_completed_run_state_is_unavailable() {
        let fake = FakeJev::start(Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "success": true,
                    "errors": [],
                    "messages": [],
                    "result": {"state": "Running", "result": null},
                }),
            )
        }))
        .await;
        let err = test_client(&fake.base_url)
            .classify(&["anything".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, JevError::Unavailable(_)), "{err:?}");
        assert_eq!(fake.calls(), 2, "one retry, then give up");
    }

    #[tokio::test]
    async fn insufficient_balance_is_unavailable_and_retried_once() {
        let fake = FakeJev::start(Arc::new(|_| {
            (
                200,
                serde_json::json!({
                    "success": false,
                    "errors": [{"code": 2021, "message": "Insufficient balance; add money to your gateway"}],
                    "messages": [],
                    "result": null,
                }),
            )
        }))
        .await;
        let err = test_client(&fake.base_url)
            .classify(&["hello".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, JevError::Unavailable(_)),
            "balance errors fail closed: {err:?}"
        );
        assert_eq!(fake.calls(), 2, "one retry, then give up");
    }

    #[tokio::test]
    async fn server_error_is_unavailable_and_retried_once() {
        let fake = FakeJev::start(Arc::new(|_| (500, serde_json::json!({"error": "boom"})))).await;
        let err = test_client(&fake.base_url)
            .classify(&["hello".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, JevError::Unavailable(_)));
        assert_eq!(fake.calls(), 2);
    }

    #[tokio::test]
    async fn timeout_is_unavailable() {
        let fake = FakeJev::start(Arc::new(|_| {
            std::thread::sleep(Duration::from_secs(5));
            (200, envelope(serde_json::json!({"answers": {}})))
        }))
        .await;
        let mut config = JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = fake.base_url.clone();
        config.timeout = Duration::from_millis(100);
        let err = JevClient::new(config)
            .unwrap()
            .classify(&["hello".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, JevError::Unavailable(_)), "{err:?}");
        assert_eq!(fake.calls(), 2, "the retry times out too");
    }

    #[tokio::test]
    async fn missing_answer_fails_closed() {
        let fake = FakeJev::start(Arc::new(|_| {
            (200, envelope(serde_json::json!({"answers": {}})))
        }))
        .await;
        let err = test_client(&fake.base_url)
            .classify(&["hello".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, JevError::Unavailable(_)));
    }

    #[tokio::test]
    async fn big_input_fans_out_with_bounded_parallelism() {
        let fake = FakeJev::start(Arc::new(|body| {
            std::thread::sleep(Duration::from_millis(100));
            (200, envelope(clear_answers(body)))
        }))
        .await;
        // 800 chars ≈ 200 tokens: 10 chunks per ~2k-token batch.
        let texts: Vec<String> = (0..100)
            .map(|i| format!("item-{i:04} ").repeat(80))
            .collect();
        let mut config = JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = fake.base_url.clone();
        config.timeout = Duration::from_secs(10);
        let scores = JevClient::new(config)
            .unwrap()
            .classify(&texts)
            .await
            .unwrap();
        assert_eq!(scores.len(), 100);
        assert_eq!(fake.calls(), 10, "100 chunks at 10 per batch");
        let max = fake.max_in_flight.load(Ordering::SeqCst);
        assert!(
            (2..=4).contains(&max),
            "batches overlap but never exceed four in flight: {max}"
        );
    }

    #[tokio::test]
    async fn small_input_is_one_call_and_empty_input_is_none() {
        let fake = FakeJev::start(Arc::new(|body| (200, envelope(clear_answers(body))))).await;
        let client = test_client(&fake.base_url);
        let scores = client
            .classify(&["short".to_string(), "texts".to_string()])
            .await
            .unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(fake.calls(), 1);
        client.classify(&[]).await.unwrap();
        assert_eq!(fake.calls(), 1, "empty input makes no calls");
    }

    #[tokio::test]
    async fn debug_redacts_the_token() {
        let client = JevClient::new(JevConfig::new(
            "acct".to_string(),
            "super-secret".to_string(),
            "outer".to_string(),
        ))
        .unwrap();
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    /// Live check against the real Jev API through AI Gateway. Needs
    /// `OUTER_TEST_JEV_ACCOUNT` / `OUTER_TEST_JEV_TOKEN` in the
    /// environment. Each text goes in its own single-chunk state, matching
    /// how the verdicts below were measured on 2026-10-23 (~0.3 s per
    /// call after a ~2.5 s first call).
    #[tokio::test]
    #[ignore = "hits the real Jev API through AI Gateway; needs OUTER_TEST_JEV_* env"]
    async fn live_classifies_sample_texts() {
        let (Some(account), Some(token)) = (
            std::env::var("OUTER_TEST_JEV_ACCOUNT").ok(),
            std::env::var("OUTER_TEST_JEV_TOKEN").ok(),
        ) else {
            panic!("OUTER_TEST_JEV_ACCOUNT and OUTER_TEST_JEV_TOKEN must both be set");
        };
        let client = JevClient::new(JevConfig::new(account, token, "outer".to_string())).unwrap();
        // (text, measured profanity, must block)
        let cases = vec![
            ("this list is fucking awesome", 0.95, true),
            ("Damn, the cock crowed at dawn", 0.28, false),
            ("essa lista é do caralho", 0.91, true),
            ("INSERT INTO items VALUES ('f u c k')", 0.91, true),
            ("Welcome to my grocery list app", 0.01, false),
            (
                "A server-rendered app, or a stored function value",
                0.01,
                false,
            ),
        ];
        for (text, measured, must_block) in cases {
            let scores = client.classify(&[text.to_string()]).await.unwrap();
            assert_eq!(
                scores[0].profanity >= 0.5,
                must_block,
                "{text:?}: measured {measured}, got {:?}",
                scores[0]
            );
        }
        // Phishing wording, measured at 0.98 on 2026-10-23.
        let scores = client
            .classify(&["Enter your bank password to verify your account".to_string()])
            .await
            .unwrap();
        assert!(
            scores[0].phishing >= 0.9,
            "credential harvesting blocks: {:?}",
            scores[0]
        );
    }
}
