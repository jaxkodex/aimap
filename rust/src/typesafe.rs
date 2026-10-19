//! The real model call behind [`crate::classifier::Asker`]: Jev through
//! TypeSafe's System One API (the Python `TypeSafeAsker` wraps the closed
//! `typesafe-sdk`; the protocol is a single JSON POST, so it is spoken
//! directly here, with [`crate::httpc`] as the transport).
//!
//! On the wire, public `bool` questions/answers are called `noul`; the
//! mapping happens at this boundary, exactly like the SDK does it, so
//! [`crate::jev`] and the stored `raw_answers` only ever see the public
//! shape.

use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::classifier::{AskError, JevResult};

/// The API base the closed SDK defaults to; `JEV_BASE_URL` overrides it
/// (tests point it at a fake server).
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1";

pub struct TypeSafeAsker {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub timeout: Duration,
}

impl TypeSafeAsker {
    pub fn new(api_key: String, model: String, base_url: String, timeout: Duration) -> TypeSafeAsker {
        TypeSafeAsker { api_key, model, base_url, timeout }
    }

    /// The endpoint the SDK posts to: `systemone` under the base URL,
    /// trailing slashes collapsed.
    pub fn endpoint(&self) -> String {
        format!("{}/systemone", self.base_url.trim_end_matches('/'))
    }

    /// The full request body: `{model, state, questions}` with bool questions
    /// in wire form.
    pub fn request_body(&self, state: &Value, questions: &Value) -> Value {
        json!({
            "model": self.model,
            "state": state,
            "questions": wire_questions(questions),
        })
    }
}

/// Public `bool` questions map to wire-level `noul`; everything else passes
/// through unchanged.
pub fn wire_questions(questions: &Value) -> Value {
    match questions {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(id, question)| {
                    let question = if question.get("type").and_then(Value::as_str) == Some("bool") {
                        let mut wire = question.clone();
                        wire["type"] = json!("noul");
                        wire
                    } else {
                        question.clone()
                    };
                    (id.clone(), question)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

impl crate::classifier::Asker for TypeSafeAsker {
    fn ask(&mut self, state: &Value, questions: &Value) -> Result<JevResult, AskError> {
        let body = self.request_body(state, questions);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| AskError::Api(format!("cannot encode the Jev request: {e}")))?;
        let headers =
            vec![("Authorization".to_string(), format!("Bearer {}", self.api_key)), ("Content-Type".to_string(), "application/json".to_string())];
        let req = crate::httpc::Request {
            method: "POST",
            url: &self.endpoint(),
            headers: &headers,
            body: Some(&payload),
            timeout: self.timeout,
        };
        let resp = crate::httpc::request(&req).map_err(|e| AskError::Api(e.to_string()))?;
        finish_ask(resp, questions, &self.model)
    }
}

impl TypeSafeAsker {
    /// The exchange over an already-connected stream (tests drive the wire
    /// through an in-memory duplex; only the connect differs in `ask`).
    pub fn ask_over(
        &self,
        stream: &mut (impl std::io::Read + std::io::Write),
        state: &Value,
        questions: &Value,
    ) -> Result<JevResult, AskError> {
        let body = self.request_body(state, questions);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| AskError::Api(format!("cannot encode the Jev request: {e}")))?;
        let headers =
            vec![("Authorization".to_string(), format!("Bearer {}", self.api_key)), ("Content-Type".to_string(), "application/json".to_string())];
        let req = crate::httpc::Request {
            method: "POST",
            url: &self.endpoint(),
            headers: &headers,
            body: Some(&payload),
            timeout: self.timeout,
        };
        let url = crate::httpc::parse_url(&self.endpoint()).map_err(|e| AskError::Api(e.to_string()))?;
        let deadline = std::time::Instant::now() + self.timeout;
        let resp = crate::httpc::exchange(stream, &req, &url, deadline)
            .map_err(|e| AskError::Api(e.to_string()))?;
        finish_ask(resp, questions, &self.model)
    }
}

fn finish_ask(resp: crate::httpc::Response, questions: &Value, model: &str) -> Result<JevResult, AskError> {
    if !(200..300).contains(&resp.status) {
        return Err(AskError::Api(format!("the Jev API returned {}", resp.status)));
    }
    let data: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| AskError::Api(format!("the Jev API returned invalid JSON: {e}")))?;
    parse_response(&data, questions, model).map_err(AskError::Api)
}

/// The answers one call must produce, checked against the questions that were
/// asked (the public form). Mirrors the SDK's validation: every question
/// answered, the right answer type, finite numbers everywhere. The `model`
/// recorded on the result is the requested model, like the SDK reports.
pub fn parse_response(data: &Value, questions: &Value, model: &str) -> Result<JevResult, String> {
    const LABEL: &str = "the Jev API";
    let answers = data
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{LABEL} returned an unexpected response"))?;
    let questions = questions
        .as_object()
        .ok_or_else(|| "Jev questions are not an object".to_string())?;

    let mut out = Map::new();
    for (id, question) in questions {
        let answer = answers
            .get(id)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{LABEL} did not return an answer for {id}"))?;
        let kind = question.get("type").and_then(Value::as_str).unwrap_or("");
        let parsed = match kind {
            "choice" => {
                if answer.get("type").and_then(Value::as_str) != Some("choice") {
                    return Err(format!("{LABEL} did not return a choice answer for {id}"));
                }
                let choice = answer
                    .get("choice")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{LABEL} did not return a choice answer for {id}"))?
                    .to_string();
                let probabilities = probabilities(LABEL, answer.get("probabilities").unwrap_or(&Value::Null), id)?;
                let confidence = required_number(LABEL, answer.get("confidence").unwrap_or(&Value::Null), &format!("confidence for {id}"))?;
                json!({"type": "choice", "choice": choice, "probabilities": probabilities, "confidence": confidence})
            }
            "score" => {
                if answer.get("type").and_then(Value::as_str) != Some("score") {
                    return Err(format!("{LABEL} did not return a score answer for {id}"));
                }
                let score = required_number(LABEL, answer.get("score").unwrap_or(&Value::Null), &format!("score for {id}"))?;
                let confidence = required_number(LABEL, answer.get("confidence").unwrap_or(&Value::Null), &format!("confidence for {id}"))?;
                json!({"type": "score", "score": score, "confidence": confidence})
            }
            // public bool, wire noul
            _ => {
                if answer.get("type").and_then(Value::as_str) != Some("noul") {
                    return Err(format!("{LABEL} did not return a bool answer for {id}"));
                }
                let probability = required_number(LABEL, answer.get("noul").unwrap_or(&Value::Null), &format!("probability for {id}"))?;
                json!({"type": "bool", "probability": probability})
            }
        };
        out.insert(id.clone(), parsed);
    }

    // A missing or malformed usage object leaves the result without usage
    // instead of failing it (the request was billed either way).
    let usage = data
        .get("usage")
        .filter(|u| u.is_object())
        .filter(|u| u.get("input_tokens").is_some() || u.get("output_tokens").is_some())
        .cloned();

    Ok(JevResult { model: model.to_string(), answers: out, usage })
}

fn required_number(label: &str, value: &Value, field: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("{label} returned an invalid {field}"))
}

fn probabilities(label: &str, value: &Value, id: &str) -> Result<Map<String, Value>, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{label} returned invalid probabilities for {id}"))?;
    let mut out = Map::new();
    for (key, probability) in map {
        let p = required_number(label, probability, &format!("probability for {id}.{key}"))?;
        out.insert(key.clone(), json!(p));
    }
    Ok(out)
}
