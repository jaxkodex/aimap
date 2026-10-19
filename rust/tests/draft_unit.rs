//! Tests for the pure parts of `src/aimap/draft.py` ported to `src/draft.rs`.
//! The Python tests exercise these through the API with `requests.post`
//! patched; here the model is faked via the `ChatTransport` trait.

use aimap::draft::{
    build_prompt, cap_thread, complete_draft, extract_reply, plan_model_call, reply_subject,
    validate_instructions, DraftConfig, DraftError, ThreadMessage, TransportError, MAX_THREAD_CHARS,
};
use serde_json::{json, Value};

fn config() -> DraftConfig {
    DraftConfig {
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("test-key".to_string()),
        model: "test-model".to_string(),
        timeout_secs: 30,
        max_tokens: 500,
        reasoning_effort: "low".to_string(),
    }
}

fn thread() -> Vec<ThreadMessage> {
    vec![
        ThreadMessage {
            message_id: 2,
            sender: Some("Bob <bob@example.com>".to_string()),
            excerpt: Some("Can you do Tuesday at 3pm?".to_string()),
        },
        ThreadMessage {
            message_id: 1,
            sender: Some("me@example.com".to_string()),
            excerpt: Some("Interview proposal attached.".to_string()),
        },
    ]
}

/// Mirrors the `MagicMock` responses in tests/test_draft.py.
struct FakeModel(Value);

impl aimap::draft::ChatTransport for FakeModel {
    fn post(&self, _call: &aimap::draft::ModelCall) -> Result<Value, TransportError> {
        Ok(self.0.clone())
    }
}

struct FailingModel {
    error: TransportError,
}

impl aimap::draft::ChatTransport for FailingModel {
    fn post(&self, _call: &aimap::draft::ModelCall) -> Result<Value, TransportError> {
        Err(self.error.clone())
    }
}

#[test]
fn prompt_matches_python_exactly() {
    // repr() of aimap.draft.build_prompt(...) for this thread, from Python:
    let expected = "You are drafting a reply to this email thread (newest message first):\n\n\
From: Bob <bob@example.com>\nCan you do Tuesday at 3pm?\n\n---\n\n\
From: me@example.com\nInterview proposal attached.\n\n\n---\n\n\
Draft a plain-text reply. Do not include quoted text, do not include a subject line. Just the body of the reply.";
    assert_eq!(build_prompt(&thread(), None), expected);

    let with_instructions = format!(
        "{}\n\nUser instructions: Keep it short",
        expected
    );
    assert_eq!(build_prompt(&thread(), Some("Keep it short")), with_instructions);
}

#[test]
fn prompt_defaults_for_missing_sender_and_excerpt() {
    let msgs = vec![ThreadMessage { message_id: 1, sender: None, excerpt: None }];
    let p = build_prompt(&msgs, None);
    assert!(p.contains("From: (unknown)\n(no body)\n"), "got {p:?}");
}

#[test]
fn reply_subject_not_doubled() {
    assert_eq!(reply_subject(Some("Interview slot")), "RE: Interview slot");
    assert_eq!(reply_subject(Some("RE: Interview slot")), "RE: Interview slot");
    assert_eq!(reply_subject(Some("re: Interview slot")), "re: Interview slot");
    assert_eq!(reply_subject(Some("  RE: Interview slot")), "  RE: Interview slot");
    assert_eq!(reply_subject(None), "RE:");
    assert_eq!(reply_subject(Some("")), "RE:");
}

#[test]
fn instructions_length_gate() {
    assert!(validate_instructions(Some(&"x".repeat(2000))).is_ok());
    assert_eq!(
        validate_instructions(Some(&"x".repeat(2001))),
        Err(DraftError::InstructionsTooLong)
    );
    // counted in code points, like Python len()
    assert_eq!(
        validate_instructions(Some(&"é".repeat(2001))),
        Err(DraftError::InstructionsTooLong)
    );
    assert!(validate_instructions(None).is_ok());
}

#[test]
fn thread_capping_respects_max_messages_and_chars() {
    // 10 messages with 5000-char excerpts: stops at MAX_THREAD_CHARS (12000)
    let msgs: Vec<ThreadMessage> = (1..=10)
        .map(|i| ThreadMessage {
            message_id: i,
            sender: Some(format!("Person{i} <p{i}@x>")),
            excerpt: Some("x".repeat(5000)),
        })
        .collect();
    let (capped, used) = cap_thread(&msgs);
    assert!(capped.len() <= 6, "MAX_THREAD_MESSAGES");
    assert_eq!(capped.len(), (MAX_THREAD_CHARS / 5000).min(6)); // 2
    assert_eq!(used, vec![1, 2]);

    // one huge excerpt falls back to the newest single message
    let msgs = vec![ThreadMessage {
        message_id: 7,
        sender: Some("a@x".to_string()),
        excerpt: Some("y".repeat(13000)),
    }];
    let (capped, used) = cap_thread(&msgs);
    assert_eq!(capped.len(), 1);
    assert_eq!(used, vec![7]);

    // missing excerpt counts as empty
    let msgs = vec![
        ThreadMessage { message_id: 1, sender: None, excerpt: None },
        ThreadMessage { message_id: 2, sender: None, excerpt: None },
    ];
    let (capped, used) = cap_thread(&msgs);
    assert_eq!(used, vec![1, 2]);
    assert_eq!(capped.len(), 2);
}

#[test]
fn happy_path_like_python_test() {
    let model = FakeModel(json!({
        "choices": [{"message": {"content": "Hi Bob,\n\nTuesday at 3pm works for me.\n\nThanks!"}}]
    }));
    let out = complete_draft(
        &config(),
        &model,
        42,
        None,
        Some("Interview slot"),
        Some("bob@example.com"),
        true,
        Some("a real person is waiting"),
        &thread(),
    )
    .unwrap();
    assert_eq!(out["message_id"], 42);
    assert_eq!(out["needs_reply"], json!(true));
    assert_eq!(out["reply_reason"], "a real person is waiting");
    assert_eq!(out["to"], json!(["bob@example.com"]));
    assert_eq!(out["cc"], json!([]));
    assert_eq!(out["subject"], "RE: Interview slot");
    assert!(out["body"].as_str().unwrap().contains("Tuesday at 3pm works"));
    assert_eq!(out["model"], "test-model");
    assert_eq!(out["used_message_ids"], json!([2, 1]));
}

#[test]
fn model_call_shape() {
    let prompt = build_prompt(&thread(), None);
    let call = plan_model_call(&config(), &prompt);
    assert_eq!(call.url, "https://api.example.com/v1/chat/completions");
    assert_eq!(call.authorization, "Bearer test-key");
    assert_eq!(call.timeout_secs, 30);
    assert_eq!(call.payload["model"], "test-model");
    assert_eq!(call.payload["max_tokens"], 500);
    assert_eq!(call.payload["temperature"], json!(0.7));
    assert_eq!(call.payload["reasoning_effort"], "low");
    assert_eq!(call.payload["messages"][0]["role"], "user");
    assert_eq!(call.payload["messages"][0]["content"], prompt);
}

#[test]
fn error_paths_match_python_messages() {
    let model = FakeModel(json!({
        "choices": [{"message": {"content": "   "}, "finish_reason": "length"}]
    }));
    let e = complete_draft(&config(), &model, 1, None, None, None, false, None, &thread()).unwrap_err();
    assert_eq!(e, DraftError::ModelRequestFailed("The model returned an empty reply.".into()));

    let model = FakeModel(json!({"error": {"message": "boom"}}));
    let e = complete_draft(&config(), &model, 1, None, None, None, false, None, &thread()).unwrap_err();
    assert_eq!(e.api_detail(), "The model response was invalid.");

    let model = FailingModel { error: TransportError::Timeout };
    let e = complete_draft(&config(), &model, 1, None, None, None, false, None, &thread()).unwrap_err();
    assert_eq!(e, DraftError::ModelTimeout);

    let model = FailingModel { error: TransportError::Failed("connection refused".into()) };
    let e = complete_draft(&config(), &model, 1, None, None, None, false, None, &thread()).unwrap_err();
    assert_eq!(e.api_detail(), "The model request failed.");
}

#[test]
fn not_configured_and_instructions_checked_before_call() {
    struct PanicTransport;
    impl aimap::draft::ChatTransport for PanicTransport {
        fn post(&self, _: &aimap::draft::ModelCall) -> Result<Value, TransportError> {
            panic!("transport must not be called");
        }
    }
    let mut cfg = config();
    cfg.api_key = None;
    assert!(!cfg.is_configured());
    let e = complete_draft(&cfg, &PanicTransport, 1, None, None, None, false, None, &thread()).unwrap_err();
    assert_eq!(e, DraftError::DraftingNotConfigured);

    let e = complete_draft(
        &config(), &PanicTransport, 1,
        Some(&"x".repeat(2001)), None, None, false, None, &thread(),
    )
    .unwrap_err();
    assert_eq!(e, DraftError::InstructionsTooLong);
}

#[test]
fn extract_reply_trims() {
    assert_eq!(
        extract_reply(&json!({"choices": [{"message": {"content": "  hello \n"}}]})).unwrap(),
        "hello"
    );
}
