use super::*;

#[test]
fn committed_rows_and_live_tail_equal_one_shot() {
    for source in [
        "# Title\n\nProse **bold** and *emphasis*.\n\nLast line.",
        "Prose\n\n```python\nprint('hello')\nprint('tail')",
        "Prose\n\n| Name | Value |\n|---|---:|\n| alpha | 1 |\n| beta | 2 |\n",
        "Prose\n\nMath $\\frac{a+b}{c}$ and $\\alpha$.\n\n$$\\sum_{i=1}^{n} i$$",
    ] {
        let expected = separated_with(source, STREAM_WIDTH, &References::new());
        for chunk in [1, 7, 512] {
            let mut stream = Stream::default();
            let mut committed = Vec::new();
            for delta in source.as_bytes().chunks(chunk) {
                stream.append(std::str::from_utf8(delta).unwrap());
                committed.extend(stream.flush());
            }
            assert!(!committed.is_empty());
            let mut visible = committed.clone();
            visible.extend_from_slice(stream.live());
            committed.extend(stream.finish());
            for width in [20, 80, 120] {
                let project = |rows: &[RenderRow]| {
                    rows.iter()
                        .flat_map(|row| row.project(width))
                        .collect::<Vec<_>>()
                };
                assert_eq!(project(&visible), project(&expected));
                assert_eq!(project(&committed), project(&expected));
            }
        }
    }

    let mut transcript = super::super::transcript::Transcript::default();
    for delta in ["a".repeat(17_000), " SUFFIX".into()] {
        let blocks = transcript.apply(super::super::session::UiEvent::Text(delta), 80);
        transcript.append(blocks, None);
        let blocks = transcript.flush_frame(80);
        transcript.append(blocks, None);
        transcript.prepare_tail(80, 0);
    }
    std::thread::sleep(Duration::from_millis(160));
    let blocks = transcript.flush_frame(80);
    transcript.append(blocks, None);
    transcript.prepare_tail(80, 0);
    assert!(
        transcript
            .tail()
            .iter()
            .flat_map(|row| row.project(80))
            .any(|line| line.to_string().contains("SUFFIX"))
    );
}

#[test]
fn math_spans_sanitize_bidi_and_tabs() {
    let source = "Inline $\\alpha\u{202e}\tx$ and display:\n\n$$\u{2066}y\tz$$\n";
    let rendered = separated_with(source, STREAM_WIDTH, &References::new());
    let flat: String = rendered
        .iter()
        .flat_map(|row| row.project(80))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !flat.contains('\u{202e}') && !flat.contains('\u{2066}') && !flat.contains('\t'),
        "unsanitized control/BiDi leaked from math: {flat:?}"
    );
}

#[test]
fn empty_notice_does_not_panic() {
    let mut transcript = super::super::transcript::Transcript::default();
    let block = transcript.notice(ratatui::text::Text::from("   "));
    assert!(block.rows.is_empty());
}

#[test]
fn markdown_links_underline_and_show_url_only_when_osc8_unavailable() {
    let source = "See [docs](https://example.com/guide) and [mail](mailto:hi@example.com).";
    let disabled = super::super::links::with_enabled(false, || {
        separated_with(source, STREAM_WIDTH, &References::new())
            .into_iter()
            .flat_map(|row| row.project(80))
            .collect::<Vec<_>>()
    });
    let disabled_text: String = disabled.iter().map(ToString::to_string).collect();
    assert!(disabled_text.contains("docs (https://example.com/guide)"));
    assert!(disabled_text.contains("mail (mailto:hi@example.com)"));
    assert!(disabled.iter().flat_map(|line| &line.spans).any(|span| {
        span.content == "docs"
            && span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
    }));

    let enabled = super::super::links::with_enabled(true, || {
        separated_with(source, STREAM_WIDTH, &References::new())
            .into_iter()
            .flat_map(|row| row.project(80))
            .collect::<Vec<_>>()
    });
    let enabled_text: String = enabled.iter().map(ToString::to_string).collect();
    assert!(enabled_text.contains("See docs and mail (mailto:hi@example.com)."));
    assert!(!enabled_text.contains("(https://example.com/guide)"));
    assert!(
        enabled
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content == "docs" && span.style.underline_color.is_some())
    );
    assert!(enabled.iter().flat_map(|line| &line.spans).any(|span| {
        span.content == "mail"
            && span.style.underline_color.is_none()
            && span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
    }));
}
