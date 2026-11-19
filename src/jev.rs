//! TypeSafe's Jev, a System One model: it reads a state and answers typed
//! questions with calibrated probabilities instead of writing text.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

pub const DEFAULT_MODEL: &str = "jev-latest";
const SYSTEM_ONE_URL: &str = "https://api.typesafe.ai/v1/systemone";

/// Questions keyed by a name of the caller's choosing. Jev answers each one
/// separately against the same state, under the same name.
pub type Questions = BTreeMap<&'static str, Question>;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// The probability that a statement is true.
    Noul {
        instructions: &'static str,
        criteria: NoulCriteria,
    },
    /// One option, with a probability for every option.
    Choice {
        instructions: &'static str,
        criteria: BTreeMap<&'static str, &'static str>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: &'static str,
    #[serde(rename = "false")]
    pub no: &'static str,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    /// Jev also names its pick and a confidence; both follow from the
    /// probabilities, so Maid reads only those.
    Choice {
        probabilities: HashMap<String, f64>,
    },
}

/// Jev's answers, with the concrete model the request resolved to.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Decision {
    pub model: String,
    pub answers: HashMap<String, Answer>,
}

#[async_trait]
pub trait SystemOne: Send + Sync {
    async fn decide(&self, state: &Value, questions: &Questions) -> Result<Decision>;
}

#[derive(Clone)]
pub struct JevClient {
    http: Client,
    url: String,
    api_key: String,
    model: String,
}

impl JevClient {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Jev HTTP client configuration should be valid"),
            url: SYSTEM_ONE_URL.to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    #[cfg(test)]
    fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }
}

#[derive(Serialize)]
struct SystemOneRequest<'a> {
    model: &'a str,
    state: &'a Value,
    questions: &'a Questions,
}

#[async_trait]
impl SystemOne for JevClient {
    async fn decide(&self, state: &Value, questions: &Questions) -> Result<Decision> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&SystemOneRequest {
                model: &self.model,
                state,
                questions,
            })
            .send()
            .await
            .context("Jev request failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jev returned {status}: {body}"));
        }
        response
            .json::<Decision>()
            .await
            .context("invalid JSON from Jev")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned response and hands back the raw request it read.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_string)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, server)
    }

    fn questions() -> Questions {
        Questions::from([
            (
                "safe",
                Question::Noul {
                    instructions: "The change is safe.",
                    criteria: NoulCriteria {
                        yes: "Nothing can break.",
                        no: "Something can break.",
                    },
                },
            ),
            (
                "size",
                Question::Choice {
                    instructions: "How big is the change?",
                    criteria: BTreeMap::from([("small", "A few lines."), ("large", "Many files.")]),
                },
            ),
        ])
    }

    #[tokio::test]
    async fn sends_typed_questions_and_reads_typed_answers() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"model":"jev-1.13.0","answers":{
                "safe":{"type":"noul","noul":0.9},
                "size":{"type":"choice","choice":"small","confidence":0.8,"probabilities":{"small":0.85,"large":0.15}}
            },"usage":{"input_tokens":10,"output_tokens":2}}"#,
        )
        .await;

        let decision = JevClient::new("secret", "jev-latest")
            .with_url(url)
            .decide(&json!({"diff": "+1"}), &questions())
            .await
            .unwrap();

        assert_eq!(decision.model, "jev-1.13.0");
        assert_eq!(decision.answers["safe"], Answer::Noul { noul: 0.9 });
        assert_eq!(
            decision.answers["size"],
            Answer::Choice {
                probabilities: HashMap::from([
                    ("small".to_string(), 0.85),
                    ("large".to_string(), 0.15)
                ]),
            }
        );
        let request = server.await.unwrap();
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret")
        );
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "jev-latest",
                "state": {"diff": "+1"},
                "questions": {
                    "safe": {
                        "type": "noul",
                        "instructions": "The change is safe.",
                        "criteria": {"true": "Nothing can break.", "false": "Something can break."}
                    },
                    "size": {
                        "type": "choice",
                        "instructions": "How big is the change?",
                        "criteria": {"small": "A few lines.", "large": "Many files."}
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn reports_the_error_jev_returns() {
        let (url, _server) = serve_once("422 Unprocessable Entity", r#"{"detail":"bad"}"#).await;

        let err = JevClient::new("secret", "jev-latest")
            .with_url(url)
            .decide(&json!("state"), &questions())
            .await
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            r#"Jev returned 422 Unprocessable Entity: {"detail":"bad"}"#
        );
    }
}
