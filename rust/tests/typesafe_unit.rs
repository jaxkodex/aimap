//! The TypeSafe System One transport: the request body mapping (public bool
//! questions to wire-level `noul`), response validation for all three answer
//! shapes, usage passthrough, and one full HTTP exchange over an in-memory
//! duplex (no sockets — loopback proved unreliable in this sandbox).

mod support;

use aimap::classifier::{AskError, JevResult};
use aimap::typesafe::{parse_response, wire_questions, TypeSafeAsker, DEFAULT_BASE_URL};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Duration;

use support::wire_pair;

fn asker(base_url: &str) -> TypeSafeAsker {
    TypeSafeAsker::new(
        "key-1".into(),
        "jev-1.13.0".into(),
        base_url.into(),
        Duration::from_secs(5),
    )
}


fn questions() -> Value {
    json!({
    "importance": {"type": "score", "scale": [0.0, 10.0]},
    "action": {"type": "choice", "choices": ["act_now", "reply", "skim", "archive"]},
    "time_sensitive": {"type": "bool"},
    })
}


#[test]
fn bool_questions_become_noul_on_the_wire() {
    let wire = wire_questions(&questions());
    assert_eq!(wire["importance"]["type"], "score");
    assert_eq!(wire["action"]["type"], "choice");
    assert_eq!(wire["time_sensitive"]["type"], "noul");
    // the public form is untouched
    assert_eq!(questions()["time_sensitive"]["type"], "bool");
}


#[test]
fn request_body_carries_model_state_and_wire_questions() {
    let a = asker(DEFAULT_BASE_URL);
    let body = a.request_body(&json!({"from": "x"}), &questions());
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["state"], json!({"from": "x"}));
    assert_eq!(body["questions"]["time_sensitive"]["type"], "noul");
    // the endpoint the closed SDK posts to
    assert_eq!(a.endpoint(), "https://api.typesafe.ai/v1/systemone");
}


#[test]
fn endpoint_collapses_trailing_slashes() {
    assert_eq!(asker("http://127.0.0.1:9/").endpoint(), "http://127.0.0.1:9/systemone");
    assert_eq!(asker("http://127.0.0.1:9/v2").endpoint(), "http://127.0.0.1:9/v2/systemone");
}


fn good_response() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "importance": {"type": "score", "score": 8.5, "confidence": 0.9},
            "action": {"type": "choice", "choice": "act_now",
                       "probabilities": {"act_now": 0.7, "reply": 0.2, "skim": 0.05, "archive": 0.05},
                       "confidence": 0.8},
            "time_sensitive": {"type": "noul", "noul": 0.95},
        },
        "usage": {"input_tokens": 1200, "output_tokens": 40},
    })
}


#[test]
fn response_parses_all_answer_shapes_and_noul_maps_back_to_bool() {
    let result = parse_response(&good_response(), &questions(), "jev-1.13.0").expect("parses");
    assert_eq!(result.model, "jev-1.13.0");
    assert_eq!(result.answers["importance"]["type"], "score");
    assert_eq!(result.answers["importance"]["score"], 8.5);
    assert_eq!(result.answers["action"]["choice"], "act_now");
    assert_eq!(result.answers["action"]["probabilities"]["act_now"], 0.7);
    assert_eq!(result.answers["time_sensitive"]["type"], "bool");
    assert_eq!(result.answers["time_sensitive"]["probability"], 0.95);
    assert_eq!(result.usage, Some(json!({"input_tokens": 1200, "output_tokens": 40})));
}


#[test]
fn usage_is_dropped_when_absent_or_malformed() {
    let mut data = good_response();
    data.as_object_mut().unwrap().remove("usage");
    let result = parse_response(&data, &questions(), "m").unwrap();
    assert_eq!(result.usage, None);

    let mut data = good_response();
    data["usage"] = json!("nope");
    assert_eq!(parse_response(&data, &questions(), "m").unwrap().usage, None);

    let mut data = good_response();
    data["usage"] = json!({});
    assert_eq!(parse_response(&data, &questions(), "m").unwrap().usage, None);
}


#[test]
fn responses_are_rejected_per_question() {
    // a missing answer
    let mut data = good_response();
    data["answers"].as_object_mut().unwrap().remove("time_sensitive");
    assert!(parse_response(&data, &questions(), "m").is_err());

    // the wrong answer type for a score question
    let mut data = good_response();
    data["answers"]["importance"] = json!({"type": "noul", "noul": 0.5});
    assert!(parse_response(&data, &questions(), "m").is_err());

    // non-finite numbers
    let mut data = good_response();
    data["answers"]["importance"]["score"] = Value::String("high".into());
    assert!(parse_response(&data, &questions(), "m").is_err());

    let mut data = good_response();
    data["answers"]["time_sensitive"]["noul"] = Value::Null;
    assert!(parse_response(&data, &questions(), "m").is_err());

    // probabilities must be numbers
    let mut data = good_response();
    data["answers"]["action"]["probabilities"] = json!({"act_now": "most"});
    assert!(parse_response(&data, &questions(), "m").is_err());

    // and answers must be an object at all
    let mut data = good_response();
    data["answers"] = json!([]);
    assert!(parse_response(&data, &questions(), "m").is_err());
}


// --------------------------------------------------------------------------------------
// the full exchange over an in-memory wire
// --------------------------------------------------------------------------------------

#[test]
fn ask_posts_the_wire_request_and_parses_the_answer() {
    let (server, client) = wire_pair();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        good_response().to_string().len(),
        good_response()
    );
    std::thread::spawn(move || {
        let mut server = server;
        // read the request head (the exchange is one-shot; Connection: close)
        let mut buf = vec![0u8; 8192];
        let n = server.read(&mut buf).expect("server reads request");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(request.starts_with("POST /v1/systemone HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Host: api.typesafe.ai\r\n"), "{request}");
        assert!(request.contains("Authorization: Bearer key-1\r\n"), "{request}");
        assert!(request.contains("Content-Type: application/json\r\n"), "{request}");
        assert!(request.contains("\"noul\""), "{request}");
        server.write_all(response.as_bytes()).expect("server writes response");
    });
    let mut client = client;
    let a = asker(DEFAULT_BASE_URL);
    let result: JevResult = a
        .ask_over(&mut client, &json!({"from": "x"}), &questions())
        .expect("the exchange completes");
    assert_eq!(result.model, "jev-1.13.0");
    assert_eq!(result.answers["time_sensitive"]["type"], "bool");
}


#[test]
fn ask_maps_http_errors_to_api_errors() {
    let (server, client) = wire_pair();
    std::thread::spawn(move || {
        let mut server = server;
        let mut buf = vec![0u8; 8192];
        let _ = server.read(&mut buf);
        server
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let mut client = client;
    let a = asker(DEFAULT_BASE_URL);
    let err = a.ask_over(&mut client, &json!({}), &questions()).unwrap_err();
    assert!(
        matches!(err, AskError::Api(ref m) if m.contains("503")),
        "{err:?}"
    );
}

