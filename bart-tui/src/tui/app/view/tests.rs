use super::*;
use crate::{
    agent::{Accounting, ContextBudget},
    history::Usage,
    provider::{Limits, Window},
    tool::test_support::TempDir,
    tui::{app::tests::attached_fixture, session::UiEvent},
};
use bart_core::conversation::UsageSummary;
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Cell,
    style::{Color, Modifier},
};
use std::num::NonZeroU64;

fn paint_status(app: &mut App, width: u16, left: &str, right: &str) -> Vec<Cell> {
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal
        .draw(|frame| {
            app.draw_frame(frame, 0);
        })
        .unwrap();
    let (row, text) = terminal
        .backend()
        .buffer()
        .content
        .chunks_exact(usize::from(width))
        .map(|row| (row, row.iter().map(Cell::symbol).collect::<String>()))
        .find(|(_, text)| text.starts_with(left))
        .expect("status row");
    let offset = usize::from(width) - right.chars().count();
    assert_eq!(
        text,
        format!(
            "{left}{:gap$}{right}",
            "",
            gap = offset - left.chars().count()
        )
    );
    row.to_vec()
}

fn assert_usage_styles(row: &[Cell], expected: &str, context: Option<(&str, Color, bool)>) {
    let mut offset = row.len() - expected.chars().count();
    for (index, part) in expected
        .split(STATUS_SEPARATOR)
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        if index > 0 {
            let end = offset + text_width(STATUS_SEPARATOR);
            for cell in &row[offset..end] {
                assert_eq!(cell.fg, Color::Reset);
                assert!(!cell.modifier.contains(Modifier::BOLD));
            }
            offset = end;
        }
        let (color, bold) = context
            .filter(|(label, ..)| *label == part)
            .map_or((Color::Reset, index == 0), |(_, color, bold)| (color, bold));
        let end = offset + text_width(part);
        for cell in &row[offset..end] {
            assert_eq!(cell.fg, color, "{part}");
            assert_eq!(cell.modifier.contains(Modifier::BOLD), bold, "{part}");
        }
        offset = end;
    }
}

#[tokio::test]
async fn status_paints_live_compaction_pressure_and_drops_low_priority_items_first() {
    let dir = TempDir::new("tui-context-budget");
    let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
    let (mut app, _) = attached_fixture(dir.path(), provider).await;
    let accounting = Accounting {
        context_tokens: 32_400,
        context_budget: Some(ContextBudget::new(NonZeroU64::new(200_000).unwrap())),
        usage: UsageSummary {
            cost: Some(0.082),
            last: Some(Usage {
                input_tokens: 100,
                cache_read_tokens: Some(85),
                cache_write_tokens: None,
                output_tokens: 0,
                reasoning_tokens: None,
                cost: None,
                cost_is_estimate: false,
            }),
            output_tokens: Some(1_200),
            ..UsageSummary::default()
        },
    };
    app.agent_event(
        UiEvent::Limits(Limits {
            short: Some(Window {
                minutes: Some(300),
                used_percent: 12.0,
            }),
            long: None,
        }),
        80,
    );
    for (tokens, wide_label, narrow_label, wide_color, narrow_color, bold) in [
        (
            32_400,
            "32k/200k",
            "16%",
            Color::Reset,
            theme().success,
            false,
        ),
        (
            134_399,
            "134k/200k",
            "67%",
            Color::Reset,
            theme().success,
            false,
        ),
        (
            134_400,
            "134k/200k",
            "67%",
            theme().literal,
            theme().literal,
            true,
        ),
        (
            159_599,
            "160k/200k",
            "80%",
            theme().literal,
            theme().literal,
            true,
        ),
        (
            159_600,
            "160k/200k",
            "80%",
            theme().error,
            theme().error,
            true,
        ),
    ] {
        app.agent_event(
            UiEvent::Accounting(Accounting {
                context_tokens: tokens,
                ..accounting
            }),
            120,
        );
        let wide_expected =
            format!("sandbox off · {wide_label} · $0.08 · 85% cached · 1.2k out · 5h 12% used");
        let row = paint_status(&mut app, 120, "● ready", &wide_expected);
        assert_usage_styles(&row, &wide_expected, Some((wide_label, wide_color, bold)));

        let narrow_expected = format!("sandbox off · {narrow_label} · $0.08");
        let row = paint_status(&mut app, 80, "● ready", &narrow_expected);
        assert_usage_styles(
            &row,
            &narrow_expected,
            Some((narrow_label, narrow_color, bold)),
        );
    }
    for (width, expected, context) in [
        (
            100,
            "sandbox off · 160k/200k · $0.08 · 85% cached · 1.2k out · 5h 12% used",
            "160k/200k",
        ),
        (99, "sandbox off · 80% · $0.08", "80%"),
        (34, "sandbox off · 80% · $0.08", "80%"),
        (26, "sandbox off · 80%", "80%"),
        (19, "sandbox off", "80%"),
        (18, "", "80%"),
    ] {
        let row = paint_status(&mut app, width, "● ready", expected);
        assert_usage_styles(&row, expected, Some((context, theme().error, true)));
    }
}

#[tokio::test]
async fn status_handles_unknown_budget_and_cost_without_output_tokens() {
    let dir = TempDir::new("tui-context-and-cost");
    let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
    let (mut app, _) = attached_fixture(dir.path(), provider).await;
    for (tokens, cost, estimate, expected) in [
        (540, Some(0.004), true, "sandbox off · ~540 · ~$0.004"),
        (1_500, Some(0.0001), false, "sandbox off · ~1.5k · <$0.001"),
        (2_000, Some(-1.0), false, "sandbox off · ~2k"),
        (999_500, Some(f64::NAN), false, "sandbox off · ~1M"),
        (1_200_000, Some(f64::INFINITY), false, "sandbox off · ~1.2M"),
        (u64::MAX, None, false, "sandbox off · ~18446744073709.6M"),
        (0, Some(-0.0), false, "sandbox off · $0.00"),
        (0, None, false, "sandbox off"),
    ] {
        app.agent_event(
            UiEvent::Accounting(Accounting {
                context_tokens: tokens,
                context_budget: None,
                usage: UsageSummary {
                    cost,
                    cost_is_estimate: estimate,
                    ..UsageSummary::default()
                },
            }),
            80,
        );
        let row = paint_status(&mut app, 80, "● ready", expected);
        assert_usage_styles(&row, expected, None);
    }
}

#[tokio::test]
async fn busy_jailed_status_keeps_recording_and_compaction_warnings() {
    use crate::confinement::{Confinement, Escape, Network};

    let dir = TempDir::new("tui-busy-jailed-status");
    let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
    let (mut app, _) = attached_fixture(dir.path(), provider).await;
    app.info.confinement = Confinement::Jailed(Network::Off, Escape::Withheld);
    for event in [
        UiEvent::Started(Vec::new()),
        UiEvent::RecordingStopped("fixture".into()),
        UiEvent::ToolBytes(5_000),
        UiEvent::Accounting(Accounting {
            context_tokens: 160_000,
            context_budget: Some(ContextBudget::new(NonZeroU64::new(200_000).unwrap())),
            usage: UsageSummary {
                cost: Some(0.08),
                ..UsageSummary::default()
            },
        }),
    ] {
        app.agent_event(event, 80);
    }
    let left = format!("REC OFF · {} tool", SPIN[app.animation().tick % SPIN.len()]);
    let right = "sandbox on, network off · 80% · $0.08";
    let row = paint_status(&mut app, 80, &left, right);
    for cell in &row[.."REC OFF".len()] {
        assert_eq!(cell.fg, theme().error);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
    assert_usage_styles(&row, right, Some(("80%", theme().error, true)));
}
