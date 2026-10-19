//! Port of `tests/test_jev.py` (unit) and the Python-generated golden vectors
//! in `fixtures/jev_golden.json`.

use aimap::jev::{
    build_questions, build_state, classifier_key, decide, header_facts, tag_examples, Decision,
    Pattern, Thresholds, NONE_OF_THESE,
};
use aimap::parse::parse_meta;
use serde_json::{json, Map, Value};

pub fn patterns() -> Vec<Pattern> {
    serde_json::from_value(json!([
        {"insight": "Shop promotions", "importance": "Low", "action_bucket": "discard",
         "tags": ["shopping", "promo"],
         "examples": [{"from": "Shop <deals@shop.example>", "subject": "20% off"}]},
        {"insight": "Client invoices", "importance": "High", "action_bucket": "act_now",
         "tags": ["work", "billing"], "examples": []},
    ]))
    .unwrap()
}

#[allow(clippy::too_many_arguments)] // mirrors the Python helper's signature
fn answers(
    pattern: &str,
    pconf: f64,
    score: f64,
    iconf: f64,
    action: &str,
    aconf: f64,
    tags: Option<Value>,
    sigs: Option<Value>,
) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("importance".into(), json!({"score": score, "confidence": iconf}));
    a.insert("action".into(), json!({"choice": action, "confidence": aconf}));
    a.insert(
        "pattern".into(),
        json!({"choice": pattern, "confidence": pconf,
               "probabilities": {"Shop promotions": 0.6, "Client invoices": 0.1, "none_of_these": 0.3}}),
    );
    let tags = tags.unwrap_or(json!({"shopping": 0.9, "promo": 0.85, "work": 0.1}));
    for (t, p) in tags.as_object().unwrap() {
        a.insert(format!("tag:{}", t), json!({"noul": p.as_f64().unwrap()}));
    }
    let sigs = sigs.unwrap_or(json!({"promotional": 1.0, "real_person": 0.0}));
    for (s, p) in sigs.as_object().unwrap() {
        a.insert(format!("sig:{}", s), json!({"noul": p.as_f64().unwrap()}));
    }
    a
}

// ---- ported unit tests -------------------------------------------------------

#[test]
fn questions_cover_patterns_tags_and_signals() {
    let pats = patterns();
    let q = build_questions(&pats);
    let criteria: Vec<&String> = q["pattern"]["criteria"].as_object().unwrap().keys().collect();
    assert_eq!(criteria.len(), 3);
    for name in ["Shop promotions", "Client invoices", NONE_OF_THESE] {
        assert!(q["pattern"]["criteria"].get(name).is_some());
    }
    for tag in ["tag:shopping", "tag:promo", "tag:work", "tag:billing"] {
        assert!(q.get(tag).is_some(), "missing {}", tag);
    }
    for (sig, _, _) in aimap::jev::SIGNALS {
        assert!(q.get(format!("sig:{}", sig)).is_some());
    }
    assert_eq!(
        q["tag:promo"]["instructions"]["emails_that_have_this_label"][0]["subject"],
        "20% off"
    );
}

#[test]
fn no_patterns_means_no_pattern_or_tag_questions() {
    let q = build_questions(&[]);
    assert!(q.get("pattern").is_none());
    assert!(!q.as_object().unwrap().keys().any(|k| k.starts_with("tag:")));
    let mut a = answers("Shop promotions", 0.9, 0.1, 0.9, "discard", 0.9, Some(json!({})), None);
    a.remove("pattern");
    let d = decide(&a, &[], &Thresholds::default()).unwrap();
    assert_eq!(d.source, "judgment");
    assert!(d.needs_review);
    assert!(d.tags.is_empty());
}

#[test]
fn confident_pattern_is_copied() {
    let a = answers("Shop promotions", 0.9, 0.1, 0.9, "discard", 0.9, None, None);
    let d = decide(&a, &patterns(), &Thresholds::default()).unwrap();
    assert_eq!(d.source, "pattern");
    assert_eq!(d.importance, "Low");
    assert_eq!(d.action_bucket, "discard");
    assert_eq!(d.tags, vec!["shopping", "promo"]);
    assert!(!d.needs_review);
    assert_eq!(d.priority, -2.8);
}

#[test]
fn disagreement_flags_review() {
    let a = answers("Shop promotions", 0.9, 0.1, 0.9, "skim", 0.9, None, None);
    let d = decide(&a, &patterns(), &Thresholds::default()).unwrap();
    assert_eq!(d.source, "pattern");
    assert!(d.needs_review);
}

#[test]
fn low_confidence_falls_back_to_judgment() {
    let a = answers(
        "Shop promotions", 0.3, 1.8, 0.9, "reply", 0.9,
        Some(json!({"work": 0.95, "promo": 0.2})), None,
    );
    let d = decide(&a, &patterns(), &Thresholds::default()).unwrap();
    assert_eq!(d.source, "judgment");
    assert_eq!(d.importance, "High");
    assert_eq!(d.action_bucket, "reply");
    assert_eq!(d.tags, vec!["work"]);
    assert_eq!(d.insight, None);
    assert!(d.needs_review);
    assert_eq!(d.signals["closest_pattern"], "Shop promotions");
}

#[test]
fn best_tag_used_when_none_passes_threshold() {
    let a = answers(
        "none_of_these", 0.9, 0.1, 0.9, "discard", 0.9,
        Some(json!({"work": 0.5, "promo": 0.3})), None,
    );
    let d = decide(&a, &patterns(), &Thresholds::default()).unwrap();
    assert_eq!(d.tags, vec!["work"]);
}

#[test]
fn classifier_key_changes_with_inputs() {
    let pats = patterns();
    let q = build_questions(&pats);
    let k = classifier_key(&q, "jev-1", &json!({"who": "Alex"}));
    assert_eq!(k, classifier_key(&build_questions(&pats), "jev-1", &json!({"who": "Alex"})));
    assert_ne!(k, classifier_key(&q, "jev-2", &json!({"who": "Alex"})));
    assert_ne!(k, classifier_key(&q, "jev-1", &json!({"who": "Sam"})));
    let first_pattern_only: Vec<Pattern> = pats[..1].to_vec();
    assert_ne!(k, classifier_key(&build_questions(&first_pattern_only), "jev-1", &json!({"who": "Alex"})));
}

#[test]
fn state_has_facts_and_body() {
    let raw = b"From: me@example.com\r\nTo: me@example.com\r\nSubject: m1\r\nDate: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m1@example.com>\r\nList-Id: x\r\n\r\nPlease confirm the meeting.\r\n";
    let meta = parse_meta(raw);
    let s = build_state(&json!({"who": "Alex"}), "me@example.com", raw, &meta, 1500);
    assert_eq!(s["recipient_profile"], json!({"who": "Alex"}));
    assert_eq!(s["email"]["facts"]["sent_by_recipient_themself"], json!(true));
    assert_eq!(s["email"]["facts"]["bulk_mail_headers"], json!(true));
    assert_eq!(s["email"]["body"], "Please confirm the meeting.");
}

// ---- Python-generated goldens -------------------------------------------------

fn golden() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/jev_golden.json"))
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn golden_questions_match_python() {
    let g = golden();
    let pats: Vec<Pattern> = serde_json::from_value(g["patterns"].clone()).unwrap();
    assert_eq!(build_questions(&pats), g["questions"]);
    assert_eq!(build_questions(&[]), g["questions_no_patterns"]);
}

#[test]
fn golden_classifier_keys_match_python() {
    let g = golden();
    let pats: Vec<Pattern> = serde_json::from_value(g["patterns"].clone()).unwrap();
    for case in g["classifier_keys"].as_array().unwrap() {
        let variant = case["questions_variant"].as_str().unwrap();
        let first_pattern_only: Vec<Pattern> = pats[..1].to_vec();
        let qs = build_questions(if variant == "first_pattern" { &first_pattern_only } else { &pats });
        let key = classifier_key(
            &qs,
            case["model"].as_str().unwrap(),
            &case["profile"],
        );
        assert_eq!(key, case["key"].as_str().unwrap(), "case {:?}", case["model"]);
    }
}

#[test]
fn golden_decisions_match_python() {
    let g = golden();
    let pats: Vec<Pattern> = serde_json::from_value(g["patterns"].clone()).unwrap();
    for case in g["decisions"].as_array().unwrap() {
        let answers: Map<String, Value> = case["answers"].as_object().unwrap().clone();
        let thresholds = match case["thresholds"] {
            Value::Null => Thresholds::default(),
            ref t => Thresholds {
                pattern_confidence: t["pattern_confidence"].as_f64().unwrap(),
                tag_threshold: t["tag_threshold"].as_f64().unwrap(),
                review_confidence: t["review_confidence"].as_f64().unwrap(),
            },
        };
        let d: Decision = decide(&answers, &pats, &thresholds).unwrap();
        let d = serde_json::to_value(&d).unwrap();
        assert_eq!(d, case["decision"], "decision case {}", case["name"]);
    }
}

#[test]
fn golden_header_facts_and_state_match_python() {
    let g = golden();
    let profile = &g["profile"];
    use base64::Engine;
    for case in g["states"].as_array().unwrap() {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(case["raw_b64"].as_str().unwrap())
            .unwrap();
        let meta = parse_meta(&raw);
        let facts = header_facts(&raw, &meta, case["account"].as_str().unwrap());
        assert_eq!(facts, case["header_facts"], "state case {}", case["name"]);
        let state = build_state(
            profile,
            case["account"].as_str().unwrap(),
            &raw,
            &meta,
            case["body_chars"].as_u64().unwrap() as usize,
        );
        assert_eq!(state, case["state"], "state case {}", case["name"]);
    }
}

#[test]
fn tag_examples_sorted_and_capped() {
    let pats = patterns();
    let ex = tag_examples(&pats, 3);
    let keys: Vec<&String> = ex.keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_eq!(ex["shopping"].as_array().unwrap().len(), 1);
}
