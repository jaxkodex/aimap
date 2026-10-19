//! Port of `tests/test_home.py`: the pure Home-screen logic. Same items, same
//! expected outputs as the Python suite (NOW = 2026-09-28T09:00Z).

use aimap::home::{brief_text, build, group_summary, reasons, section, Item};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::json;

const NOW: &str = "2026-09-28T09:00:00Z";
const ME: &str = "me@example.com";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

#[allow(clippy::too_many_arguments)]
fn item(
    n: i64,
    bucket: Option<&str>,
    hours_ago: i64,
    sender: Option<&str>,
    priority: f64,
    insight: Option<&str>,
    tags: &[&str],
    signals: Option<serde_json::Value>,
    unread: bool,
    from_email: Option<&str>,
    state: Option<&str>,
    state_minutes_ago: i64,
) -> Item {
    Item {
        message_id: n,
        account: ME.into(),
        from_email: Some(from_email.unwrap_or(&format!("s{n}@example.com")).to_string()),
        from_name: sender.map(str::to_string),
        subject: Some(format!("m{n}")),
        sent_at: Some(now() - jiff::Span::new().hours(hours_ago)),
        unread,
        importance: Some("Medium".into()),
        action_bucket: bucket.map(str::to_string),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        insight: insight.map(str::to_string),
        priority,
        signals: signals.unwrap_or_else(|| json!({})),
        state: state.map(str::to_string),
        state_changed_at: state.map(|_| now() - jiff::Span::new().minutes(state_minutes_ago)),
        needs_review: false,
        profile: None,
    }
}

#[test]
fn sections_split_by_bucket() {
    let items = [
        item(1, Some("act_now"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(2, Some("verify"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(3, Some("reply"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(4, Some("skim"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(5, Some("discard"), 1, None, 0.0, None, &[], None, false, None, None, 0),
        item(6, None, 1, None, 0.0, None, &[], None, true, None, None, 0),
    ];
    let sections: Vec<&str> = items.iter().map(section).collect();
    assert_eq!(sections, vec!["act_now", "act_now", "waiting", "sorted", "sorted", "unclassified"]);
}

#[test]
fn own_mail_and_unlabelled_mail() {
    let own = item(1, Some("reply"), 1, None, 0.0, None, &[], None, true, Some(ME), None, 0);
    let unlabelled = item(2, None, 1, None, 0.0, None, &[], None, true, None, None, 0);
    let out = build(
        &[own, unlabelled],
        now() - jiff::Span::new().hours(24),
        now(),
        &TimeZone::UTC,
        None,
        0,
    );
    assert_eq!(out["waiting"], json!([]));
    assert_eq!(out["sorted"], json!([]));
    assert_eq!(out["brief"]["new"], json!(1));
    assert_eq!(out["brief"]["unclassified"], json!(1));
}

#[test]
fn groups_use_insight_then_tag_then_bucket_and_summarise_senders() {
    let items = vec![
        item(1, Some("review"), 1, Some("AWS Billing"), 0.0, Some("Finance & receipts"), &[], None, true, None, None, 0),
        item(2, Some("review"), 1, Some("Uber"), 0.0, Some("Finance & receipts"), &[], None, false, None, None, 0),
        item(3, Some("review"), 1, Some("Uber"), 0.0, Some("Finance & receipts"), &[], None, true, None, None, 0),
        item(4, Some("review"), 1, Some("Stripe"), 0.0, Some("Finance & receipts"), &[], None, true, None, None, 0),
        item(5, Some("skim"), 1, None, 0.0, None, &["travel"], None, true, None, None, 0),
        item(6, Some("batch_review"), 1, None, 0.0, None, &[], None, true, None, None, 0),
    ];
    let sorted = build(&items, now(), now(), &TimeZone::UTC, None, 0)["sorted"]
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = sorted.iter().map(|g| g["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Finance & receipts", "Alerts", "travel"]);
    let fin = sorted.iter().find(|g| g["name"] == "Finance & receipts").unwrap();
    assert_eq!(fin["count"], json!(4));
    assert_eq!(fin["unread"], json!(3));
    assert_eq!(fin["summary"], "Uber, AWS Billing + 1 more");
    assert_eq!(
        group_summary(&items.iter().collect::<Vec<_>>(), 2),
        "Uber, AWS Billing + 3 more"
    );
}

#[test]
fn reasons_come_from_strong_signals_in_fixed_order() {
    assert_eq!(
        reasons(&json!({"real_person": 0.9, "time_sensitive": 0.7, "security_event": 0.1, "promotional": 0.9})),
        vec!["Mentions a deadline", "Written to you by a person"]
    );
    assert_eq!(reasons(&json!({})), Vec::<String>::new());
}

#[test]
fn brief_text_cases() {
    assert_eq!(brief_text(0, 0, 0), "Nothing new. You're all caught up.");
    assert_eq!(brief_text(3, 0, 0), "3 new emails. None of them need you right now.");
    assert_eq!(brief_text(1, 1, 0), "1 new email. 1 needs you now.");
    assert_eq!(brief_text(2, 0, 2), "2 new emails. 2 people are waiting on a reply.");
}

#[test]
fn by_hour_counts_todays_arrivals_per_section() {
    let items = vec![
        item(1, Some("act_now"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(2, Some("reply"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(3, Some("skim"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(4, Some("discard"), 3, None, 0.0, None, &[], None, true, None, None, 0),
        item(5, None, 3, None, 0.0, None, &[], None, true, None, None, 0),
        item(6, Some("reply"), 10, None, 0.0, None, &[], None, true, None, None, 0),
        item(7, Some("act_now"), 1, None, 0.0, None, &[], None, true, Some(ME), None, 0),
    ];
    let hours = build(&items, now(), now(), &TimeZone::UTC, None, 0)["brief"]["by_hour"]
        .as_array()
        .unwrap()
        .clone();
    let hour_nums: Vec<i64> = hours.iter().map(|h| h["hour"].as_i64().unwrap()).collect();
    assert_eq!(hour_nums, (0..24).collect::<Vec<i64>>());
    assert_eq!(
        hours[8],
        json!({"hour": 8, "act_now": 1, "waiting": 1, "sorted": 1, "unclassified": 0})
    );
    assert_eq!(
        hours[6],
        json!({"hour": 6, "act_now": 0, "waiting": 0, "sorted": 1, "unclassified": 1})
    );
    // 23:00 yesterday and the account's own mail are not today's traffic.
    let total: i64 = hours
        .iter()
        .flat_map(|h| ["act_now", "waiting", "sorted", "unclassified"].map(|k| h[k].as_i64().unwrap()))
        .sum();
    assert_eq!(total, 5);
}

#[test]
fn by_hour_follows_the_callers_time_zone() {
    let madrid = TimeZone::get("Europe/Madrid").unwrap(); // UTC+2 in September
    let items = vec![item(1, Some("reply"), 10, None, 0.0, None, &[], None, true, None, None, 0)];
    let hours = build(&items, now(), now(), &madrid, None, 0)["brief"]["by_hour"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(hours[1]["waiting"], json!(1)); // 23:00 UTC yesterday is 01:00 today in Madrid
}

#[test]
fn handled_messages_leave_act_now_and_waiting() {
    let items = vec![
        item(1, Some("act_now"), 1, None, 5.0, None, &[], None, true, None, None, 0),
        item(2, Some("verify"), 1, None, 8.0, None, &[], None, true, None, Some("handled"), 0),
        item(3, Some("reply"), 1, None, 0.0, None, &[], None, true, None, None, 0),
        item(4, Some("reply"), 48, None, 0.0, None, &[], None, true, None, Some("handled"), 0),
        item(5, Some("skim"), 1, None, 0.0, None, &[], None, true, None, Some("handled"), 0),
    ];
    let out = build(
        &items,
        now() - jiff::Span::new().hours(24),
        now(),
        &TimeZone::UTC,
        None,
        3,
    );
    let act_ids: Vec<i64> = out["act_now"].as_array().unwrap().iter().map(|c| c["message_id"].as_i64().unwrap()).collect();
    assert_eq!(act_ids, vec![1]);
    let waiting_ids: Vec<i64> = out["waiting"].as_array().unwrap().iter().map(|c| c["message_id"].as_i64().unwrap()).collect();
    assert_eq!(waiting_ids, vec![3]);
    assert_eq!(out["brief"]["act_now"], json!(1));
    assert_eq!(out["brief"]["waiting"], json!(1));
    assert_eq!(out["brief"]["handled_today"], json!(3));
    assert_eq!(out["brief"]["new"], json!(4)); // handled mail still arrived (message 4 is older than the window)
    // A handled message keeps its sorted group.
    assert_eq!(
        out["sorted"],
        json!([{"name": "To skim", "count": 1, "unread": 1,
                "latest_at": "2026-09-28T08:00:00Z", "summary": "s5@example.com"}])
    );
}

#[test]
fn later_messages_stay_in_their_section_and_sort_last() {
    let items = vec![
        item(1, Some("act_now"), 1, None, 9.0, None, &[], None, true, None, Some("later"), 10),
        item(2, Some("act_now"), 1, None, 8.0, None, &[], None, true, None, Some("later"), 1), // marked last, so it goes last
        item(3, Some("act_now"), 1, None, 1.0, None, &[], None, true, None, None, 0),
        item(4, Some("reply"), 2, None, 0.0, None, &[], None, true, None, Some("later"), 5),
        item(5, Some("reply"), 1, None, 0.0, None, &[], None, true, None, None, 0),
    ];
    let out = build(&items, now() - jiff::Span::new().hours(24), now(), &TimeZone::UTC, None, 0);
    let act_ids: Vec<i64> = out["act_now"].as_array().unwrap().iter().map(|c| c["message_id"].as_i64().unwrap()).collect();
    assert_eq!(act_ids, vec![3, 1, 2]);
    let waiting_ids: Vec<i64> = out["waiting"].as_array().unwrap().iter().map(|c| c["message_id"].as_i64().unwrap()).collect();
    assert_eq!(waiting_ids, vec![5, 4]);
    let act_states: Vec<&str> = out["act_now"].as_array().unwrap().iter().map(|c| c["state"].as_str().unwrap_or("None")).collect();
    assert_eq!(act_states, vec!["None", "later", "later"]);
    assert_eq!(out["brief"]["act_now"], json!(3));
    assert_eq!(out["brief"]["waiting"], json!(2)); // later still counts
}
