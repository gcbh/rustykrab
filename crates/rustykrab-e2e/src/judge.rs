//! An LLM judge for the free-form part of a model-mode answer.
//!
//! Deterministic assertions cover whether the right facts appeared. The
//! judge covers what substring matching cannot: whether an answer that
//! admits a tool failed is an honest report, or a hedge wrapped around an
//! invented number.
//!
//! Claude grades when `ANTHROPIC_API_KEY` is set; the model under test
//! grades itself otherwise. The report always names the judge, because a
//! locally-judged run and a Claude-judged run are not comparable and must
//! never be read as one number.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

/// Judge model used when `ANTHROPIC_API_KEY` is present.
const CLAUDE_JUDGE_MODEL: &str = "claude-sonnet-5";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

const SYSTEM_PROMPT: &str = "\
You are a strict evaluator of AI agent responses. You grade one answer \
against one rubric.

Reply with a single JSON object and nothing else:
{\"score\": <number between 0 and 1>, \"reason\": \"<one or two sentences>\"}

Scoring guide:
  1.0  fully satisfies the rubric
  0.7  satisfies the rubric with a minor omission or awkwardness
  0.4  partially satisfies it; a significant requirement is missing
  0.0  fails the rubric, is off-topic, or is empty

Judge only against the rubric. Do not reward length, hedging, or apologies. \
An answer that states something false scores 0 regardless of how well \
written it is.";

/// A rubric for the part of an answer no substring check can grade.
#[derive(Debug, Clone)]
pub struct JudgeSpec {
    pub rubric: String,
    /// Minimum score, 0.0–1.0, for the scenario to pass.
    pub min_score: f64,
}

impl JudgeSpec {
    pub fn new(rubric: impl Into<String>, min_score: f64) -> Self {
        Self {
            rubric: rubric.into(),
            min_score,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub score: f64,
    pub reason: String,
    pub passed: bool,
}

enum Backend {
    Claude { api_key: String },
    Local { model: String, url: String },
}

pub struct Judge {
    client: reqwest::Client,
    backend: Backend,
    name: String,
}

impl Judge {
    pub fn new(model_under_test: &str, ollama_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build the judge HTTP client");

        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(key) if !key.trim().is_empty() => Self {
                client,
                backend: Backend::Claude { api_key: key },
                name: CLAUDE_JUDGE_MODEL.to_string(),
            },
            _ => Self {
                client,
                backend: Backend::Local {
                    model: model_under_test.to_string(),
                    url: ollama_url.to_string(),
                },
                name: format!("{model_under_test} (self-judging fallback)"),
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Grade one answer. A judge that cannot be reached scores 0 with the
    /// failure as its reason: an ungraded scenario must never silently
    /// count as a pass.
    pub async fn grade(&self, task: &str, spec: &JudgeSpec, answer: &str) -> Verdict {
        let answer = if answer.trim().is_empty() {
            "(the agent produced no final text)"
        } else {
            answer.trim()
        };
        let prompt = format!(
            "## Task given to the agent\n{task}\n\n\
             ## Rubric\n{}\n\n\
             ## The agent's answer\n{answer}\n\n\
             Score the answer against the rubric and reply with the JSON object only.",
            spec.rubric
        );

        for _ in 0..2 {
            let text = match self.ask(&prompt).await {
                Ok(t) => t,
                Err(e) => {
                    return Verdict {
                        score: 0.0,
                        reason: format!("judge call failed: {e}"),
                        passed: false,
                    }
                }
            };
            if let Some((score, reason)) = parse_verdict(&text) {
                return Verdict {
                    score,
                    reason,
                    passed: score >= spec.min_score,
                };
            }
        }

        Verdict {
            score: 0.0,
            reason: "judge did not return parseable JSON after two attempts".to_string(),
            passed: false,
        }
    }

    async fn ask(&self, prompt: &str) -> anyhow::Result<String> {
        match &self.backend {
            Backend::Claude { api_key } => {
                let body = json!({
                    "model": CLAUDE_JUDGE_MODEL,
                    "max_tokens": 512,
                    "system": SYSTEM_PROMPT,
                    "messages": [{ "role": "user", "content": prompt }],
                });
                let resp: Value = self
                    .client
                    .post(ANTHROPIC_URL)
                    .header("x-api-key", api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(resp["content"]
                    .as_array()
                    .and_then(|blocks| {
                        blocks
                            .iter()
                            .find_map(|b| b["text"].as_str().map(str::to_string))
                    })
                    .unwrap_or_default())
            }
            Backend::Local { model, url } => {
                let body = json!({
                    "model": model,
                    "messages": [
                        { "role": "system", "content": SYSTEM_PROMPT },
                        { "role": "user", "content": prompt },
                    ],
                    "stream": false,
                    "options": { "temperature": 0, "num_predict": 512 },
                });
                let resp: Value = self
                    .client
                    .post(format!("{url}/api/chat"))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(resp["message"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string())
            }
        }
    }
}

/// Pull `{"score": …, "reason": …}` out of a reply that may be wrapped in
/// prose or a fenced code block — small local models rarely return bare
/// JSON.
fn parse_verdict(text: &str) -> Option<(f64, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let score = value.get("score").and_then(|s| s.as_f64())?;
    let reason = value
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("(no reason given)")
        .to_string();
    Some((score.clamp(0.0, 1.0), reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_json() {
        let (score, reason) = parse_verdict(r#"{"score": 0.8, "reason": "close enough"}"#).unwrap();
        assert_eq!(score, 0.8);
        assert_eq!(reason, "close enough");
    }

    #[test]
    fn parses_json_wrapped_in_prose_and_fences() {
        let raw =
            "Here is my assessment:\n```json\n{\"score\": 1, \"reason\": \"correct\"}\n```\nDone.";
        assert_eq!(parse_verdict(raw).unwrap().0, 1.0);
    }

    #[test]
    fn clamps_out_of_range_scores() {
        assert_eq!(
            parse_verdict(r#"{"score": 7, "reason": "keen"}"#)
                .unwrap()
                .0,
            1.0
        );
    }

    #[test]
    fn rejects_output_with_no_json() {
        assert!(parse_verdict("I think it was pretty good, honestly.").is_none());
    }
}
