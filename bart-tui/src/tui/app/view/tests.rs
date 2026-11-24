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

mod cell_click_and_hover {
    use super::*;
    use crate::{
        history::{
            Activity, Completeness, JobOutcome, Notification, NotificationKind, SavedArtifact,
        },
        preferences::Ui,
        tool::{LiveState, Status},
        tui::{
            app::{Effect, Msg},
            input::ReadEvent,
            tasks::{Copy, Task},
            transcript::MAX_DETAILS,
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn input(app: &mut App, event: Event) -> Vec<Effect> {
        app.update(Msg::Input(ReadEvent::Terminal(event)))
    }

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
        input(app, Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) -> Vec<Effect> {
        input(
            app,
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }),
        )
    }

    fn paint(app: &mut App) -> Terminal<TestBackend> {
        paint_height(app, 24)
    }

    fn paint_height(app: &mut App, height: u16) -> Terminal<TestBackend> {
        app.update(Msg::Size(ratatui::layout::Size::new(80, height)));
        app.update(Msg::PrepareFrame);
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        let painted = app.render_frame(&mut terminal, 0).unwrap();
        app.update(Msg::Painted(Some(painted)));
        terminal
    }

    fn locate(terminal: &Terminal<TestBackend>, token: &str) -> (u16, u16) {
        let buf = terminal.backend().buffer();
        let width = usize::from(buf.area.width);
        let height = usize::from(buf.area.height);
        for y in 0..height {
            let text: String = buf.content[y * width..(y + 1) * width]
                .iter()
                .map(Cell::symbol)
                .collect();
            if let Some(byte) = text.find(token) {
                let col = text[..byte].chars().count();
                return (u16::try_from(col).unwrap(), u16::try_from(y).unwrap());
            }
        }
        panic!("token {token:?} not found");
    }

    mod transcript_search {
        use super::*;

        fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
            key(app, code, KeyModifiers::NONE)
        }

        fn open(app: &mut App) {
            key(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        }

        fn paste(app: &mut App, text: &str) {
            input(app, Event::Paste(text.into()));
        }

        fn painted(app: &mut App) -> String {
            frame_text(&paint(app))
        }

        fn frame_text(terminal: &Terminal<TestBackend>) -> String {
            terminal
                .backend()
                .buffer()
                .content
                .as_chunks::<80>()
                .0
                .iter()
                .map(|row| row.iter().map(Cell::symbol).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn user(app: &mut App, text: &str) {
            app.update(Msg::Events(vec![UiEvent::Delivering {
                text: text.into(),
                skills: vec![],
            }]));
        }

        fn history(app: &mut App) {
            for i in 0..100 {
                if [10, 40, 70].contains(&i) {
                    user(app, &format!("needle {i}"));
                } else {
                    user(app, &format!("line {i}"));
                }
            }
        }

        #[tokio::test]
        async fn modal_find_mode_owns_navigation_with_nonempty_composer_draft() {
            let dir = TempDir::new("tui-transcript-search-input");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            user(&mut app, "a needle and another NEEDLE");
            press(&mut app, KeyCode::Char('/'));
            assert!(painted(&mut app).contains("start a new session"));
            app.drafts.composer.clear();
            app.insert("unfinished draft");

            open(&mut app);
            for c in "NeX".chars() {
                press(&mut app, KeyCode::Char(c));
            }
            assert!(painted(&mut app).contains("0 matches /NeX"));
            press(&mut app, KeyCode::Backspace);
            paste(&mut app, "edle");
            let frame = painted(&mut app);
            assert!(frame.contains("2 matches /Needle"));
            assert!(frame.contains("Enter find") && frame.contains("Esc cancel"));
            for invalid in ["bad\nline".into(), "x".repeat(1025), "\x1b[2J".into()] {
                paste(&mut app, &invalid);
                let frame = painted(&mut app);
                assert!(
                    frame.contains("paste refused: search requires one line within 1024 bytes")
                );
                assert!(frame.contains("2 matches /Needle"));
            }

            // Enter transitions to modal Find Navigation — n/N/Enter/Shift+Enter/arrows step
            // matches without touching the preserved composer draft!
            press(&mut app, KeyCode::Enter);
            let frame = painted(&mut app);
            assert!(frame.contains("find /Needle [2/2]"));
            press(&mut app, KeyCode::Char('n'));
            assert!(painted(&mut app).contains("find /Needle [1/2]"));
            key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
            assert!(painted(&mut app).contains("find /Needle [2/2]"));
            press(&mut app, KeyCode::Char('z'));
            paste(&mut app, "ignored paste");
            assert_eq!(app.drafts.composer.text(), "unfinished draft");

            // Ctrl+C exits Find Mode without erasing the preserved composer draft.
            key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
            assert!(!app.scrollback.search().in_mode());
            assert_eq!(app.drafts.composer.text(), "unfinished draft");
            app.quit_armed = None;

            // Re-enter Find Mode and verify a stale post-resize click does not exit Find Mode.
            open(&mut app);
            paste(&mut app, "Needle");
            press(&mut app, KeyCode::Enter);
            let term = paint(&mut app);
            let stale_draft_pos = locate(&term, "unfinished");
            input(&mut app, Event::Resize(80, 16));
            mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                stale_draft_pos,
            );
            mouse(
                &mut app,
                MouseEventKind::Up(MouseButton::Left),
                stale_draft_pos,
            );
            assert!(app.scrollback.search().in_mode());
            assert!(!app.drafts.composer.selecting());

            // Clicking inside the painted composer exits Find Mode back to editing the draft.
            let term = paint(&mut app);
            let draft_pos = locate(&term, "unfinished");
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), draft_pos);
            mouse(&mut app, MouseEventKind::Up(MouseButton::Left), draft_pos);
            assert!(!app.scrollback.search().in_mode());
            press(&mut app, KeyCode::Char('!'));
            assert_eq!(app.drafts.composer.text(), "unfinished draft!");

            open(&mut app);
            paste(&mut app, "absent");
            press(&mut app, KeyCode::Enter);
            assert!(painted(&mut app).contains("No matches"));
            assert_eq!(app.drafts.composer.text(), "unfinished draft!");
        }

        #[tokio::test]
        async fn search_at_bottom_keeps_following_and_slash_opens_commands() {
            let dir = TempDir::new("tui-search-follow");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            user(&mut app, "one needle");
            open(&mut app);
            paste(&mut app, "needle");
            press(&mut app, KeyCode::Enter);
            assert!(painted(&mut app).contains("find /needle [1/1]"));
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('/'));
            assert!(painted(&mut app).contains("start a new session"));
        }

        #[tokio::test]
        async fn scrolling_down_at_the_bottom_keeps_slash_commands() {
            let dir = TempDir::new("tui-scroll-down-follow");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            user(&mut app, "latest message");
            assert!(painted(&mut app).contains("latest message"));
            mouse(&mut app, MouseEventKind::ScrollDown, (0, 3));
            press(&mut app, KeyCode::Char('/'));
            assert!(painted(&mut app).contains("start a new session"));
        }

        #[tokio::test]
        async fn page_up_during_streaming_uses_the_last_painted_page() {
            let dir = TempDir::new("tui-scroll-painted-origin");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            history(&mut app);
            let initial = painted(&mut app);
            assert!(initial.contains("line 99") && !initial.contains("line 85"));

            press(&mut app, KeyCode::PageUp);
            for i in 0..40 {
                user(&mut app, &format!("stream message {i}"));
            }
            let scrolled = painted(&mut app);
            assert!(scrolled.contains("line 85") && !scrolled.contains("stream message"));

            press(&mut app, KeyCode::PageUp);
            key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
            assert!(painted(&mut app).contains("stream message 39"));
        }

        #[tokio::test]
        async fn fast_search_steps_are_applied_before_the_first_paint() {
            let dir = TempDir::new("tui-search-fast-steps");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            history(&mut app);
            for (direction, modifiers, token, position) in [
                ('n', KeyModifiers::NONE, "needle 10", "[1/3]"),
                ('N', KeyModifiers::SHIFT, "needle 40", "[2/3]"),
            ] {
                key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
                open(&mut app);
                paste(&mut app, "needle");
                press(&mut app, KeyCode::Enter);
                key(&mut app, KeyCode::Char(direction), modifiers);
                assert!(app.drafts.composer.is_empty());
                let frame = painted(&mut app);
                assert!(frame.contains(token) && frame.contains(position));
            }
        }

        #[tokio::test]
        async fn search_and_latest_keys_respect_unpainted_scroll_order() {
            let dir = TempDir::new("tui-search-scroll-order");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            history(&mut app);
            assert!(painted(&mut app).contains("line 99"));
            press(&mut app, KeyCode::PageUp);
            open(&mut app);
            paste(&mut app, "needle");
            press(&mut app, KeyCode::Enter);
            assert!(painted(&mut app).contains("needle 70"));

            open(&mut app);
            paste(&mut app, "needle");
            assert!(!painted(&mut app).contains("line 99"));
            key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
            let frame = painted(&mut app);
            assert!(frame.contains("line 99") && frame.contains("3 matches /needle"));

            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Char('N'));
            key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
            let frame = painted(&mut app);
            assert!(frame.contains("line 99") && frame.contains("find /needle"));
        }

        #[tokio::test]
        async fn page_down_after_search_seek_shows_the_same_rows_with_or_without_a_paint() {
            let dir = TempDir::new("tui-search-page-down-order");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            history(&mut app);
            let [separate, batched] = [true, false].map(|paint_between| {
                key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
                assert!(painted(&mut app).contains("line 99"));
                open(&mut app);
                paste(&mut app, "needle");
                press(&mut app, KeyCode::Enter);
                if paint_between {
                    assert!(painted(&mut app).contains("needle 70"));
                }
                press(&mut app, KeyCode::PageDown);
                let frame = painted(&mut app);
                let area = app.geometry.conversation;
                frame
                    .lines()
                    .skip(usize::from(area.y))
                    .take(usize::from(area.height))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            assert!(!separate.contains("needle 70"));
            assert_eq!(batched, separate);
        }

        #[tokio::test]
        async fn short_search_layout_keeps_a_transcript_row() {
            let dir = TempDir::new("tui-search-short-layout");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            app.geometry.size = ratatui::layout::Size::new(80, 8);
            user(&mut app, "needle in the haystack");
            for draft in ["", "draft\nline two\nline three\nline four\nline five"] {
                app.drafts.composer.clear();
                if !draft.is_empty() {
                    app.update(Msg::Events(vec![UiEvent::Started(vec![])]));
                    for message in ["queued one", "queued two", "queued three"] {
                        app.insert(message);
                        press(&mut app, KeyCode::Tab);
                    }
                    assert_eq!(app.drafts.queued.len(), 3);
                }
                app.insert(draft);
                open(&mut app);
                paste(&mut app, "needle");
                let frame = frame_text(&paint_height(&mut app, 8));
                assert!(
                    frame.contains("needle in the haystack") && frame.contains("1 matches /needle")
                );
                press(&mut app, KeyCode::Enter);
                let frame = frame_text(&paint_height(&mut app, 8));
                assert!(
                    frame.contains("needle in the haystack")
                        && frame.contains("find /needle [1/1]")
                );
                assert_eq!(app.drafts.composer.text(), draft);
                press(&mut app, KeyCode::Esc);
            }
        }

        #[tokio::test]
        async fn search_wraps_through_history_and_keeps_manual_scrolling() {
            let dir = TempDir::new("tui-transcript-search-history");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            history(&mut app);
            assert!(painted(&mut app).contains("line 99"));
            open(&mut app);
            paste(&mut app, "needle");
            press(&mut app, KeyCode::Enter);
            let frame = painted(&mut app);
            assert!(frame.contains("find /needle [3/3]"));
            assert!(frame.contains("needle 70"));
            assert!(frame.contains("line 69"));
            press(&mut app, KeyCode::Char('n'));
            let frame = painted(&mut app);
            assert!(frame.contains("needle 10"));
            assert!(frame.contains("find /needle [1/3]"));
            key(&mut app, KeyCode::Char('N'), KeyModifiers::SHIFT);
            assert!(painted(&mut app).contains("needle 70"));
            press(&mut app, KeyCode::Up);
            assert!(painted(&mut app).contains("needle 40"));
            press(&mut app, KeyCode::PageDown);
            assert!(!painted(&mut app).contains("needle 40"));
            press(&mut app, KeyCode::PageUp);
            let match_row = |frame: &str| frame.lines().position(|row| row.contains("needle 40"));
            let row = match_row(&painted(&mut app)).unwrap();
            mouse(&mut app, MouseEventKind::ScrollUp, (0, 3));
            assert_eq!(match_row(&painted(&mut app)), Some(row + 1));
            mouse(&mut app, MouseEventKind::ScrollDown, (0, 3));
            assert_eq!(match_row(&painted(&mut app)), Some(row));
            key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
            let frame = painted(&mut app);
            assert!(frame.contains("line 99"));
            assert!(frame.contains("find /needle [2/3]"));
            press(&mut app, KeyCode::Char('/'));
            assert!(painted(&mut app).contains("0 matches /"));
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::PageUp);
            paint(&mut app);
            press(&mut app, KeyCode::Char('/'));
            assert!(painted(&mut app).contains("0 matches /"));
            assert!(app.drafts.composer.is_empty());
        }

        #[tokio::test]
        async fn search_highlights_do_not_change_columns_or_drag_copy() {
            let dir = TempDir::new("tui-transcript-search-grapheme");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            let value = "e\u{301}界Z";
            user(&mut app, value);
            let term = paint(&mut app);
            let at = locate(&term, "e\u{301}界");
            open(&mut app);
            paste(&mut app, "\u{301}");
            assert!(painted(&mut app).contains("1 matches /"));
            press(&mut app, KeyCode::Enter);
            let terminal = paint(&mut app);
            assert_eq!(locate(&terminal, "e\u{301}界"), at);
            let row = &terminal.backend().buffer().content.as_chunks::<80>().0[usize::from(at.1)];
            let cell = &row[usize::from(at.0)];
            assert_eq!(cell.symbol(), "e\u{301}");
            assert!(cell.modifier.contains(Modifier::BOLD | Modifier::REVERSED));
            assert_eq!(row[usize::from(at.0) + 1].symbol(), "界");
            assert!(
                !row[usize::from(at.0) + 1]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
            let end = (at.0 + 3, at.1);
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), at);
            mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), end);
            let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), end);
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::Spawn(_, Task::Copy(Copy::Text { text, .. })) if text == value
            )));
        }

        #[tokio::test]
        async fn switching_sessions_discards_the_active_search() {
            let dir = TempDir::new("tui-transcript-search-session");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            user(&mut app, "needle");
            open(&mut app);
            paste(&mut app, "needle");
            press(&mut app, KeyCode::Enter);
            assert!(painted(&mut app).contains("find /needle [1/1]"));
            let next = app
                .shared
                .opener
                .open(
                    app.info.settings.clone(),
                    app.info.preferences.clone(),
                    None,
                )
                .await
                .unwrap();
            app.update(Msg::Attached(crate::tui::tasks::opened(next).await));
            let frame = painted(&mut app);
            assert!(!frame.contains("find /") && !frame.contains("needle"));
        }

        #[tokio::test]
        async fn search_escape_does_not_interrupt_and_rewind_clears_it() {
            let dir = TempDir::new("tui-transcript-search-escape");
            let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
            let (mut app, _) = attached_fixture(dir.path(), provider).await;
            user(&mut app, "needle");
            app.update(Msg::Events(vec![UiEvent::Started(vec![])]));
            app.agent_event(
                UiEvent::WaitingForReset {
                    message: "wait".into(),
                    delay: std::time::Duration::from_secs(60),
                },
                80,
            );
            assert!(
                app.turn
                    .as_ref()
                    .is_some_and(|turn| turn.progress.waiting_for_reset())
            );
            for submit in [false, true] {
                open(&mut app);
                paste(&mut app, "needle");
                if submit {
                    press(&mut app, KeyCode::Enter);
                    assert!(painted(&mut app).contains("find /needle"));
                }
                assert!(press(&mut app, KeyCode::Esc).is_empty());
                let frame = painted(&mut app);
                assert!(!frame.contains("find /") && !frame.contains("matches /"));
                assert!(
                    app.turn
                        .as_ref()
                        .is_some_and(|turn| turn.progress.waiting_for_reset())
                );
            }
            open(&mut app);
            paste(&mut app, "needle");
            press(&mut app, KeyCode::Enter);
            assert!(painted(&mut app).contains("find /needle [1/1]"));
            let (ack, _) = tokio::sync::oneshot::channel();
            app.update(Msg::Events(vec![UiEvent::Ended {
                ack,
                error: None,
                recover_input: false,
                rejected: vec![],
            }]));
            press(&mut app, KeyCode::Esc);
            paste(&mut app, "/rewind");
            assert!(
                press(&mut app, KeyCode::Enter)
                    .iter()
                    .any(|effect| matches!(
                        effect,
                        Effect::Send(crate::tui::session::Command::Messages)
                    ))
            );
            open(&mut app);
            paste(&mut app, "needle");
            app.update(Msg::Events(vec![UiEvent::Rewound(Ok((
                "restored".into(),
                Box::default(),
                Accounting::default(),
            )))]));
            let frame = painted(&mut app);
            assert!(!frame.contains("find /") && !frame.contains("needle"));
            assert_eq!(app.drafts.composer.text(), "restored");
        }
    }

    fn is_underlined(terminal: &Terminal<TestBackend>, token: &str) -> bool {
        let (col, row) = locate(terminal, token);
        let buf = terminal.backend().buffer();
        let start = usize::from(row) * usize::from(buf.area.width) + usize::from(col);
        let end = start + token.chars().count();
        buf.content[start..end]
            .iter()
            .all(|cell| cell.modifier.contains(Modifier::UNDERLINED))
    }

    fn assert_viewer_output(app: &mut App, wanted: &str) {
        assert!(matches!(app.surface, Some(Surface::Viewer(..))));
        let _ = paint(app);
        let effects = key(app, KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::Spawn(_, Task::Copy(Copy::Text { text, .. })) if text == wanted
            )),
            "expected viewer copy of {wanted:?}"
        );
        key(app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.surface.is_none());
    }

    #[tokio::test]
    async fn hovering_underlines_clickable_cells_and_clicking_opens_them_without_breaking_drag() {
        let dir = TempDir::new("tui-cell-hover-click");
        let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
        let (mut app, _) = attached_fixture(dir.path(), provider).await;
        app.transcript.set_ui(Ui::Cells);
        app.update(Msg::Events(vec![
            UiEvent::ToolStarted {
                id: "first".into(),
                name: "first".into(),
                arguments: serde_json::json!({"code": "read('a.rs')"}).into(),
                live: None,
            },
            UiEvent::ToolFinished {
                id: "first".into(),
                output: "first output".into(),
                status: Status::Succeeded,
                elapsed: None,
                jobs: Vec::new(),
                diffs: Vec::new(),
                activities: crate::tool::Activities {
                    rows: vec![Activity {
                        title: "read('a.rs')".into(),
                        state: LiveState::Finished,
                    }],
                    omitted: 0,
                },
            },
            UiEvent::ToolStarted {
                id: "second".into(),
                name: "second".into(),
                arguments: serde_json::json!({"code": "run('test')"}).into(),
                live: None,
            },
            UiEvent::ToolFinished {
                id: "second".into(),
                output: "failure preview\nproblem".into(),
                status: Status::Failed,
                elapsed: None,
                jobs: Vec::new(),
                diffs: Vec::new(),
                activities: crate::tool::Activities {
                    rows: vec![Activity {
                        title: "run('test')".into(),
                        state: LiveState::Failed,
                    }],
                    omitted: 0,
                },
            },
            UiEvent::Compacted("compaction body".into()),
            UiEvent::Notification(Notification {
                text: "[job 1]\njob detail".into(),
                kind: NotificationKind::JobFinished {
                    id: 1,
                    outcome: JobOutcome::Exit(0),
                    command: "echo ok".into(),
                    capture: SavedArtifact {
                        path: "job.log".into(),
                        completeness: Completeness::Complete,
                        chars: 0,
                        bytes: 0,
                    },
                },
            }),
        ]));

        let term = paint(&mut app);
        assert!(!is_underlined(&term, "first"));
        let first_pos = locate(&term, "first");

        // Hover underlines the hovered cell only.
        mouse(&mut app, MouseEventKind::Moved, first_pos);
        let term = paint(&mut app);
        assert!(is_underlined(&term, "first"));
        assert!(!is_underlined(&term, "second"));

        // Moving outside conversation clears hover underline.
        let outside = (0, app.geometry.conversation.bottom());
        mouse(&mut app, MouseEventKind::Moved, outside);
        let term = paint(&mut app);
        assert!(!is_underlined(&term, "first"));

        // Dragging text on a cell row copies without opening the viewer.
        let drag_end = (first_pos.0 + 4, first_pos.1);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), first_pos);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), drag_end);
        let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), drag_end);
        assert!(app.surface.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Spawn(_, Task::Copy(Copy::Text { text, .. })) if text == "first"
        )));

        // Clicking opens the exact cell or notice directly.
        for (token, expected) in [
            ("first", "first output"),
            ("second", "failure preview\nproblem"),
            ("Context compacted", "compaction body"),
            ("job 1 finished", "job detail"),
        ] {
            let term = paint(&mut app);
            let pos = locate(&term, token);
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), pos);
            mouse(&mut app, MouseEventKind::Up(MouseButton::Left), pos);
            assert_viewer_output(&mut app, expected);
        }

        // Ctrl+O keeps its standard behavior (opens latest detail, not failed cell).
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_viewer_output(&mut app, "job detail");

        // Evicted detail shows status instead of panicking or opening wrong item.
        for _ in 0..MAX_DETAILS {
            app.transcript.note("filler", "filler");
        }
        let term = paint(&mut app);
        let pos = locate(&term, "first");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), pos);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), pos);
        assert!(app.surface.is_none());
        assert_eq!(app.status.text(), Some("detail is no longer retained"));

        // Drag updates against stale geometry after resize are ignored so they cannot pull in a resized tool row.
        app.update(Msg::Events(vec![
            UiEvent::Text("Before".into()),
            UiEvent::Responded,
        ]));
        let term = paint(&mut app);
        let prose_pos = locate(&term, "Before");
        let cell_pos = locate(&term, "1 line");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), prose_pos);
        app.update(Msg::Size(ratatui::layout::Size::new(120, 24)));
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), cell_pos);
        let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), cell_pos);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Spawn(_, Task::Copy(_))))
        );

        // Stationary pointer hover follows the row under the pointer when content appends.
        let term = paint(&mut app);
        let job_pos = locate(&term, "job 1 finished");
        mouse(&mut app, MouseEventKind::Moved, job_pos);
        let term = paint(&mut app);
        assert!(is_underlined(&term, "job 1 finished"));
        let filler = "row\n".repeat(12);
        app.update(Msg::Events(vec![
            UiEvent::Text(format!("```txt\n{filler}```")),
            UiEvent::Responded,
        ]));
        let term = paint(&mut app);
        assert!(locate(&term, "job 1 finished").1 < job_pos.1);
        assert!(!is_underlined(&term, "job 1 finished"));

        // Oversized compaction replay summary evicted by bound_details does not panic.
        let oversized = "x".repeat(33 * 1024 * 1024);
        app.update(Msg::Events(vec![UiEvent::Compacted(oversized)]));
    }
}
