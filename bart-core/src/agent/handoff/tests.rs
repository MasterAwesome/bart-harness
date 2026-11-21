use super::*;
use crate::history::{Assistant, Part, ToolCall, ToolResult};

fn plan(history: &[Message], window: u64, instructions: &str) -> Result<Plan, Error> {
    Plan::new(history, &[], window, instructions, 1, String::new(), &[])
}

fn user(text: &str) -> Message {
    Message::User(text.into())
}
fn step(id: &str) -> Message {
    Message::Assistant(Assistant {
        protocol: provider::Protocol::Responses,
        provider: "p".into(),
        model: "m".into(),
        parts: vec![
            Part {
                content: Content::Reasoning("reasoning".into()),
                replay: Some(serde_json::json!({"encrypted_content":"opaque replay"}).into()),
            },
            Part {
                content: Content::ToolCall(ToolCall {
                    id: id.into(),
                    name: "python".into(),
                    arguments: serde_json::json!({"code":"print(1)"}).into(),
                }),
                replay: None,
            },
        ],
        usage: None,
        continues: false,
        refused: false,
        served_model: None,
    })
}
fn result(id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResult {
        activities: Default::default(),
        diffs: Vec::new(),
        call_id: id.into(),
        output: text.into(),
        is_error: false,
        notes: String::new(),
        jobs: vec![],
        saved: None,
        cell: None,
    })
}
#[test]
fn invalid_or_unfinished_exchanges_and_oversized_atomic_inputs_are_rejected() {
    for history in [
        vec![step("a")],
        vec![result("a", "orphan")],
        vec![step("a"), result("a", "1"), step("a"), result("a", "2")],
        vec![step("a"), result("a", &"x".repeat(300_000))],
    ] {
        assert!(plan(&history, 64_000, "system").is_err());
    }
    let Message::Assistant(mut a) = step("a") else {
        panic!()
    };
    a.parts.clear();
    a.continues = true;
    assert!(plan(&[Message::Assistant(a)], 64_000, "system").is_err());
}

#[test]
fn summary_fragments_preserve_unicode_and_obey_the_input_bound() {
    for text in ["🦀α\n".repeat(70_000), "\0\n\"\\🦀".repeat(30_000)] {
        let budget = 10_000;
        let split = segments(&[user(&text)], budget, None).unwrap();
        let mut restored = String::new();
        for segment in split {
            assert!(segment.iter().map(compact::estimate).sum::<u64>() <= budget);
            let [Message::User(fragment)] = segment.as_slice() else {
                panic!()
            };
            restored.push_str(fragment.split_once('\n').unwrap().1);
        }
        assert_eq!(restored, text);
    }
}

#[test]
fn summaries_cannot_overrun_the_planned_carry_budget_in_a_small_window() {
    let plan = plan(&[user("Read-only audit")], 10_000, "system").unwrap();
    let oversized = "x".repeat((plan.output_tokens as usize + 1) * 4);
    assert!(plan.finish(&oversized, 1, "system").is_err());
}

#[test]
fn interrupted_pause_turn_followed_by_user_message_can_be_compacted() {
    let Message::Assistant(mut paused) = step("a") else {
        panic!()
    };
    paused.parts.clear();
    paused.continues = true;
    let history = [
        user("initial"),
        Message::Assistant(paused),
        user("new direction after interrupt"),
    ];
    assert!(plan(&history, 64_000, "system").is_ok());
}

#[test]
fn oversized_user_source_splits_when_window_shrinks() {
    let source = Message::UserSource(crate::history::UserSource {
        text: "🦀α\n".repeat(70_000).into(),
        archive: 1,
        index: 0,
    });
    let budget = 10_000;
    let split = segments(&[source], budget, None).unwrap();
    assert!(split.len() > 1);
    for segment in split {
        assert!(segment.iter().map(compact::estimate).sum::<u64>() <= budget);
    }
}

#[test]
fn provider_reported_usage_prevents_false_two_stage_split_and_tail_retains_recent_exchanges() {
    let mut first = step("c1");
    if let Message::Assistant(a) = &mut first {
        a.parts[0].replay =
            Some(serde_json::json!({"encrypted_content": "e".repeat(120_000)}).into());
    }
    let mut second = step("c2");
    if let Message::Assistant(a) = &mut second {
        a.parts[0].replay =
            Some(serde_json::json!({"encrypted_content": "e".repeat(120_000)}).into());
        a.usage = Some(crate::history::Usage {
            input_tokens: 4_000,
            output_tokens: 200,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost: None,
            cost_is_estimate: false,
        });
    }
    let history = [
        user("task"),
        first,
        result("c1", "step 1 ok"),
        second,
        result("c2", "step 2 ok"),
        step("c3"),
        result("c3", "step 3 ok"),
        step("c4"),
        result("c4", "step 4 ok"),
    ];
    assert!(plan(&history, 64_000, "system").unwrap().segments.len() > 1);
    let conversation = crate::conversation::Conversation::of(history.to_vec());
    let crate::conversation::Anchor::Reported { after, usage } = conversation.context().anchor()
    else {
        panic!()
    };
    let reported = crate::conversation::Estimate {
        messages: after,
        tokens: usage.input_tokens + usage.output_tokens,
    };
    let planned = Plan::new(
        &history,
        &[reported],
        64_000,
        "system",
        1,
        String::new(),
        &[],
    )
    .unwrap();
    assert_eq!(
        planned.segments.len(),
        1,
        "authoritative provider usage within budget must keep a single compaction stage"
    );
    assert_eq!(
        planned.tail.len(),
        4,
        "consecutive small recent tool exchanges in the active turn should stay together in tail"
    );
}
