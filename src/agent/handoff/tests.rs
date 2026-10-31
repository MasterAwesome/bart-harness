use super::*;
use crate::history::{Assistant, Part, ToolCall, ToolResult};

fn plan(history: &[Message], window: u64, instructions: &str) -> Result<Plan, Error> {
    Plan::new(history, window, instructions, 1, String::new(), &[])
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
        let split = segments(&[user(&text)], budget).unwrap();
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
