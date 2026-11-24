use super::*;

#[test]
fn committed_rows_and_live_tail_equal_one_shot() {
    for source in [
        "# Title\n\nProse **bold** and *emphasis*.\n\nLast line.",
        "Prose\n\n```python\nprint('hello')\nprint('tail')",
        "```python\nvalue = '''first\n    second\nthird'''\n\n  final",
        "~~~\n    a very long first code line    \n\n    final\n~~~\n",
        "Prose\n\n| Name | Value |\n|---|---:|\n| alpha | 1 |\n| beta | 2 |\n",
        "Prose\n\nMath $\\frac{a+b}{c}$ and $\\alpha$.\n\n$$\\sum_{i=1}^{n} i$$",
    ] {
        let expected = separated_with(source, &References::new());
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
        let blocks = transcript.flush_frame();
        transcript.append(blocks, None);
        transcript.prepare_tail(80, 0);
    }
    std::thread::sleep(Duration::from_millis(160));
    let blocks = transcript.flush_frame();
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
fn committed_quoted_table_keeps_its_layout_when_body_rows_grow() {
    let initial = "> > > > > | H |\n> > > > > | --- |\n> > > > > | first |\n> > > > > | second |\n";
    for (width, prefix, value) in [
        (11, "…─", "界".to_owned()),
        (32, "│ │ │ │ │ ─", "x".repeat(11_000)),
    ] {
        let project = |rows: &[RenderRow]| {
            selectable(rows, 0, width)
                .into_iter()
                .map(Row::into_line)
                .collect::<Vec<_>>()
        };
        let mut stream = Stream::default();
        stream.append(initial);
        let mut committed = stream.flush();
        let mut initial_visible = committed.clone();
        initial_visible.extend_from_slice(stream.live());
        let first = project(&initial_visible);
        assert!(first.iter().any(|row| row.to_string().contains("first")));
        assert!(first.iter().any(|row| row.to_string().starts_with(prefix)));

        let delta = format!("> > > > > | {value} |\n");
        stream.append(&delta);
        committed.extend(stream.flush());
        let mut visible = committed.clone();
        visible.extend_from_slice(stream.live());
        committed.extend(stream.finish());
        let expected = separated_with(&format!("{initial}{delta}"), &References::new());
        let expected = project(&expected);
        assert_eq!(project(&visible), expected);
        assert_eq!(project(&committed), expected);
    }
}

#[test]
fn math_spans_sanitize_bidi_and_tabs() {
    let source = "Inline $\\alpha\u{202e}\tx$ and display:\n\n$$\u{2066}y\tz$$\n";
    let rendered = separated_with(source, &References::new());
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
        separated_with(source, &References::new())
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
        separated_with(source, &References::new())
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

const TABLE_FIXTURE: &str =
    "| Name | Data |\n| :---: | ---: |\n| x | abcdefg   hij klm |\n| z | a \\| b |";

fn hit(rows: &[Row], needle: &str, last: bool) -> Point {
    rows.iter()
        .find_map(|row| {
            let line = row.line().to_string();
            line.find(needle).map(|byte| {
                let col =
                    text::width(&line[..byte]) + if last { text::width(needle) - 1 } else { 0 };
                selection::point_at(row, col)
            })
        })
        .expect(needle)
}

fn reversed(rows: &[Row], range: (Point, Point)) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            let selected: String = selection::highlight(row, range)
                .spans
                .iter()
                .filter(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                })
                .map(|span| span.content.as_ref())
                .collect();
            (!selected.is_empty()).then_some(selected)
        })
        .collect()
}

#[test]
fn boxed_cell_drag_rejoins_dropped_spaces_and_snaps_inside_its_box() {
    let rows = render_detail(TABLE_FIXTURE, 13);
    let start = hit(&rows, "bcdefg", false);
    let end = hit(&rows, "klm", true);
    assert_eq!(
        selection::text(&rows, (start, end)).as_deref(),
        Some("bcdefg   hij klm")
    );
    assert_eq!(reversed(&rows, (start, end)), ["bcdefg", "hij klm"]);
    let first = rows
        .iter()
        .position(|row| row.line().to_string().contains("abcdefg"))
        .unwrap();
    let next = &rows[first + 1];
    let x = hit(&rows, "x", false);
    for column in [0, 1, 3, 4] {
        let padding = selection::point_at(next, column);
        assert_eq!(selection::text(&rows, (x, padding)).as_deref(), Some("x"));
        assert_eq!(reversed(&rows, (x, padding)), ["x"]);
    }
    let last = selection::point_at(next, 12);
    assert_eq!(last, end);
}

#[test]
fn stacked_labels_are_chrome_and_table_coordinates_survive_reflow() {
    let boxed = render_detail(TABLE_FIXTURE, 13);
    let stacked = render_detail(TABLE_FIXTURE, 7);
    let first = hit(&stacked, "Data:", false);
    let last = hit(&stacked, "klm", true);
    assert_eq!(first, hit(&boxed, "abcdefg", false));
    assert_eq!(last, hit(&boxed, "klm", true));
    for rows in [&boxed, &stacked] {
        assert_eq!(
            selection::text(rows, (first, last)).as_deref(),
            Some("abcdefg   hij klm")
        );
        assert!(
            reversed(rows, (first, last))
                .iter()
                .all(|line| !line.contains("Data:") && !line.contains('─') && !line.contains('╌'))
        );
    }
    let row = (hit(&stacked, "x", false), last);
    assert_eq!(
        selection::text(&stacked, row).as_deref(),
        Some("| x | abcdefg   hij klm |")
    );
}

#[test]
fn border_drag_and_prose_drag_copy_gfm_rows_without_highlighting_chrome() {
    let source = format!("Above\n\n{TABLE_FIXTURE}\n\nBelow");
    let rows = render_detail(&source, 28);
    let full = (hit(&rows, "Name", false), hit(&rows, "a | b", true));
    assert_eq!(selection::text(&rows, full).as_deref(), Some(TABLE_FIXTURE));
    assert!(
        reversed(&rows, full)
            .iter()
            .all(|text| !text.contains(['─', '╌']))
    );
    let stacked = render_detail(&source, 7);
    for rows in [&rows, &stacked] {
        let range = (hit(rows, "Above", false), hit(rows, "Below", true));
        assert_eq!(
            selection::text(rows, range),
            Some(format!("Above\n\n{TABLE_FIXTURE}\n\nBelow"))
        );
        let cross_rows = (hit(rows, "Name", false), hit(rows, "abcdefg", false));
        assert_eq!(
            selection::text(rows, cross_rows).as_deref(),
            Some("| Name | Data |\n| :---: | ---: |\n| x | abcdefg   hij klm |")
        );
    }
    assert_eq!(
        render_filtered(TABLE_FIXTURE, &References::new())
            .iter()
            .map(RenderRow::source)
            .collect::<Vec<_>>()
            .join("\n"),
        TABLE_FIXTURE
    );
}

#[test]
fn margins_and_quote_prefixes_do_not_leak_into_wrapped_prose_or_links() {
    super::super::links::with_enabled(true, || {
        let url = "https://example.com/long";
        let mut source = render_filtered(&format!("> {url}"), &References::new());
        for row in &mut source {
            row.prefix(Span::raw("  "));
        }
        let rows = selectable(&source, 0, 13);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| {
            row.line().to_string().starts_with("  │ ")
                && row.line().spans[..2]
                    .iter()
                    .all(|span| span.style.underline_color.is_none())
                && row.line().spans[2..]
                    .iter()
                    .all(|span| span.style.underline_color.is_some())
        }));
        let range = (
            selection::point_at(&rows[0], 0),
            selection::point_at(rows.last().unwrap(), 13),
        );
        assert_eq!(selection::text(&rows, range).as_deref(), Some(url));
        assert_eq!(reversed(&rows, range).concat(), url);
        for width in 1..=4 {
            let rows = selectable(&source, 0, width);
            assert!(
                rows.iter()
                    .all(|row| row.line().width() <= usize::from(width))
            );
            assert_eq!(
                selection::text(
                    &rows,
                    (
                        selection::point_at(&rows[0], 0),
                        selection::point_at(rows.last().unwrap(), usize::from(width))
                    )
                )
                .as_deref(),
                Some(url)
            );
        }
    });
}

#[test]
fn list_and_footnote_align_continuations_without_copying_indent() {
    for (source, marker) in [
        (
            "10. First paragraph with enough words.\n\n    Another paragraph.",
            "10. ",
        ),
        (
            "- First paragraph with enough words.\n\n  Another paragraph.",
            "- ",
        ),
        (
            "[^1]: First paragraph with enough words.\n\n    Another paragraph.",
            "[^1]: ",
        ),
    ] {
        let rows = render_detail(source, 16);
        let first = hit(&rows, marker, false);
        let last = hit(&rows, "paragraph.", true);
        let selected = selection::text(&rows, (first, last)).unwrap();
        assert!(
            selected.starts_with(&format!("{marker}First paragraph with enough words.\n")),
            "{selected:?}"
        );
        assert!(selected.ends_with("\nAnother paragraph."), "{selected:?}");
        let continuation = " ".repeat(marker.len());
        assert!(
            rows.iter()
                .skip(1)
                .all(|row| row.line().to_string().starts_with(&continuation))
        );
        let without_marker = hit(&rows, "First", false);
        assert!(
            selection::text(&rows, (without_marker, last))
                .unwrap()
                .starts_with("First paragraph")
        );
    }
}

#[test]
fn quoted_list_code_copies_verbatim_across_soft_wraps() {
    let source =
        "> - Example:\n>\n>   ```text\n>       alpha    beta   \n>   \n>         gamma\n>   ```";
    let mut rows = render_filtered(source, &References::new());
    for row in &mut rows {
        row.prefix(Span::raw("  "));
    }
    let display = selectable(&rows, 1, 15);
    let first = display
        .iter()
        .find(|row| row.line().to_string().contains("alph"))
        .unwrap();
    assert!(first.line().to_string().starts_with("  │       "));
    let range = (
        selection::point_at(first, 0),
        selection::point_at(display.last().unwrap(), 15),
    );
    assert_eq!(
        selection::text(&display, range).as_deref(),
        Some("    alpha    beta   \n\n      gamma")
    );
    assert_eq!(
        rows.iter()
            .map(RenderRow::source)
            .collect::<Vec<_>>()
            .join("\n"),
        "- Example:\n    alpha    beta   \n\n      gamma"
    );
}

#[test]
fn table_selection_counts_graphemes_across_styles_and_wide_cells() {
    let rows = render_detail(
        "| Heading | Other |\n| ---: | --- |\n| **e**\u{301}界字 | next |",
        24,
    );
    let row = rows
        .iter()
        .find(|row| row.line().to_string().contains("界字"))
        .unwrap();
    let display = row.line().to_string();
    let at = text::width(&display[..display.find('界').unwrap()]);
    let left_padding = selection::point_at(row, 1);
    let wide_character = selection::point_at(row, at + 1);
    assert_eq!(
        selection::text(&rows, (left_padding, wide_character)).as_deref(),
        Some("e\u{301}界")
    );
    assert_eq!(
        reversed(&rows, (left_padding, wide_character)),
        ["e\u{301}界"]
    );
}

#[test]
fn prose_fills_rows_without_showing_wrap_boundary_spaces_on_the_next_line() {
    let rows = render_detail("hello   world\nnext", 5);
    assert_eq!(
        rows.iter()
            .map(|row| row.line().to_string())
            .collect::<Vec<_>>(),
        ["hello", "world", "next"]
    );
    let range = (hit(&rows, "hello", false), hit(&rows, "next", true));
    assert_eq!(
        selection::text(&rows, range).as_deref(),
        Some("hello   world\nnext")
    );
}

#[test]
fn stale_offsets_inside_a_table_cell_do_not_slice_a_glyph() {
    let source = "| Name |\n| --- |\n| ab |";
    let old = render_detail(source, 15);
    let range = (hit(&old, "ab", false), hit(&old, "ab", true));
    let new = render_detail(&source.replace("ab", "界"), 15);
    assert_eq!(selection::text(&new, range), None);
}

#[test]
fn prefixes_leave_room_to_paint_wide_glyphs_in_prose_and_stacked_cells() {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    for source in ["> 界", "> | 界 |\n> | --- |"] {
        for (width, column) in [(2, 0), (3, 1)] {
            let rows = render_detail(source, width);
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        selection::borrowed(rows[0].line()),
                        Rect::new(0, 0, width, 1),
                    );
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(column, 0)].symbol(), "界");
            let at = selection::point_at(&rows[0], usize::from(column));
            assert_eq!(selection::text(&rows, (at, at)).as_deref(), Some("界"));
        }
    }
}

#[test]
fn multiline_math_and_depth_fallback_keep_line_breaks_in_copy() {
    let math = render_detail("$$\na\nb\n$$", 12);
    assert_eq!(
        selection::text(&math, (hit(&math, "a", false), hit(&math, "b", true))).as_deref(),
        Some("a\nb")
    );
    let source = format!("{}line one\nline two", "> ".repeat(33));
    let fallback = render_detail(&source, 100);
    assert_eq!(
        selection::text(
            &fallback,
            (
                hit(&fallback, "line one", false),
                hit(&fallback, "line two", true)
            )
        )
        .as_deref(),
        Some("line one\nline two")
    );
}

#[test]
fn copied_gfm_round_trips_inline_code_delimiters_pipes_and_literal_entities() {
    let source = r"| Value | Other |
| --- | --- |
| C:\Users\me | next |
| `C:\*logs*` | next |
| a \| b | next |
| a &#92;\| b | next |
| a &#92;&#92;\| b | next |
| &amp;#92; | next |";
    let rows = render_detail(source, 36);
    let full = (hit(&rows, "Value", false), hit(&rows, "&#92;", true));
    let last_next = selection::point_at(rows.last().unwrap(), 35);
    let copied = selection::text(&rows, (full.0, last_next)).unwrap();
    assert!(copied.contains(r"| C:\Users\me | next |"));

    let mut decoded = Vec::<String>::new();
    for event in Parser::new_ext(&copied, OPTIONS) {
        match event {
            Event::Start(Tag::TableCell) => decoded.push(String::new()),
            Event::Text(text) | Event::Code(text) => decoded.last_mut().unwrap().push_str(&text),
            _ => {}
        }
    }
    assert_eq!(
        decoded,
        [
            "Value",
            "Other",
            r"C:\Users\me",
            "next",
            r"C:\*logs*",
            "next",
            "a | b",
            "next",
            r"a \| b",
            "next",
            r"a \\| b",
            "next",
            "&#92;",
            "next",
        ]
    );
}

#[test]
fn reverse_drag_via_empty_cell_selects_the_row_without_adjacent_rows() {
    let rows = render_detail(
        "| Left | Right |\n| --- | --- |\n| | alpha |\n| z | other |",
        21,
    );
    let body = rows
        .iter()
        .find(|row| row.line().to_string().contains("alpha"))
        .unwrap();
    let empty = selection::point_at(body, 2);
    let right = hit(&rows, "alpha", true);
    let (a, b) = (right, empty);
    let range = (a.min(b), a.max(b));
    assert_eq!(
        selection::text(&rows, range).as_deref(),
        Some("|  | alpha |")
    );
    assert_eq!(reversed(&rows, range), ["alpha"]);
    assert_eq!(selection::text(&rows, (empty, empty)), None);
}

#[test]
fn clipped_body_row_is_copied_between_retained_rows() {
    let huge = "x".repeat(MAX_TABLE_ROW_BYTES + 1);
    let source = format!("| Head |\n| --- |\n| before |\n| {huge} |\n| after |");
    let rows = render_filtered(&source, &References::new());
    let display = selectable(&rows, 0, 22);
    let range = (hit(&display, "Head", false), hit(&display, "after", true));
    let expected = format!("| Head |\n| --- |\n| before |\n{TABLE_BYTES_NOTICE}\n| after |");
    assert_eq!(
        selection::text(&display, range).as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(
        rows.iter()
            .map(RenderRow::source)
            .collect::<Vec<_>>()
            .join("\n"),
        expected
    );
    let rows = render_detail(&format!("| {huge} |\n| --- |"), 20);
    assert_eq!(rows.len(), 1);
}

#[test]
fn stacked_table_uses_one_projection_budget_and_keeps_following_prose() {
    let mut markdown = String::from("| Label |\n| --- |\n");
    let value = "x".repeat(200);
    markdown.push_str(&format!("| {value} |\n").repeat(200));
    markdown.push_str("\nAfter the table");
    let rows = render_detail(&markdown, 1);
    let (table, tail) = rows.split_at(MAX_TABLE_RENDERED_ROWS);
    let (notice, retained) = table.split_last().unwrap();
    assert_eq!(table[0].line().to_string(), "L");
    assert_eq!(notice.line().to_string(), "…");
    let last_body = selection::point_at(retained.last().unwrap(), 0);
    let notice_point = selection::point_at(notice, 0);
    assert_eq!(
        selection::text(&rows, (last_body, notice_point)).unwrap(),
        format!("| {value} |\n{TABLE_ROWS_NOTICE}")
    );
    let end = selection::point_at(tail.last().unwrap(), 0);
    assert_eq!(
        selection::text(&rows, (notice_point, end)).unwrap(),
        format!("{TABLE_ROWS_NOTICE}\n\nAfter the table")
    );
    assert_eq!(
        tail.iter()
            .map(|row| row.line().to_string())
            .collect::<String>(),
        "Afterthetable"
    );
}

#[test]
fn table_search_excludes_matches_wholly_in_a_clipped_cell_suffix() {
    let last_visible = MAX_TABLE_RENDERED_ROWS - 3;
    let value = "a".repeat(last_visible - 2);
    let rows = render_detail(&format!("| H |\n| --- |\n| {value}XYsuffix |"), 1);
    assert_eq!(rows.last().unwrap().line().to_string(), "…");
    assert_eq!(rows[rows.len() - 2].line().to_string(), "Y");

    let hits = selection::search(&rows, "suffix");
    assert!(hits.is_empty());
    let hits = selection::search(&rows, "xys");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].anchor, rows[rows.len() - 3].anchor);
    assert_eq!(
        rows.iter()
            .filter(|row| !selection::marks(row, &hits, 3).is_empty())
            .count(),
        2
    );
}

#[test]
fn wide_wrap_separator_keeps_copy_hit_testing_and_search_in_source_order() {
    let text = "a\u{3000}b";
    let rows = render_detail(text, 2);
    assert_eq!(
        rows.iter()
            .map(|row| row.line().to_string())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let a = selection::point_at(&rows[0], 0);
    let b = selection::point_at(&rows[1], 0);
    assert!(a < b);
    assert_eq!(selection::text(&rows, (b, b)).as_deref(), Some("b"));
    assert_eq!(selection::text(&rows, (a, b)).as_deref(), Some(text));
    let hits = selection::search(&rows, text);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].anchor, rows[0].anchor);
    for row in &rows {
        assert_eq!(
            selection::marks(row, &hits, text.len()),
            std::slice::from_ref(&(0..1))
        );
    }
}

#[test]
fn nested_quote_table_uses_booktabs_rules_and_natural_widths() {
    let source = "> > > > > | H |\n> > > > > | --- |\n> > > > > | 界 |";
    let boxed = render_detail(source, 4);
    assert_eq!(
        boxed
            .iter()
            .map(|row| row.line().to_string())
            .collect::<Vec<_>>(),
        ["…H", "…──", "…界"]
    );
    let range = (hit(&boxed, "H", false), hit(&boxed, "界", true));
    let table = "| H |\n| --- |\n| 界 |";
    assert_eq!(selection::text(&boxed, range).as_deref(), Some(table));
    let stacked = render_detail(source, 1);
    assert_eq!(
        stacked
            .iter()
            .map(|row| row.line().to_string())
            .collect::<Vec<_>>(),
        ["H", "…", "界"]
    );
    assert_eq!(selection::text(&stacked, range).as_deref(), Some(table));
}

#[test]
fn booktabs_table_adds_dashed_dividers_only_when_rows_wrap() {
    let source = "| Svc | Notes | Ms |\n| --- | --- | ---: |\n| api | Validates JWT tokens and keys | 12 |\n| job | Retries webhook deliveries | 48 |";
    let wide: Vec<_> = render_detail(source, 60)
        .into_iter()
        .map(|row| row.line().to_string())
        .collect();
    assert_eq!(
        wide,
        [
            "Svc  Notes                          Ms",
            "──────────────────────────────────────",
            "api  Validates JWT tokens and keys  12",
            "job  Retries webhook deliveries     48",
        ]
    );
    let narrow: Vec<_> = render_detail(source, 26)
        .into_iter()
        .map(|row| row.line().to_string())
        .collect();
    assert!(narrow.iter().any(|line| line.starts_with('─')));
    assert!(narrow.iter().any(|line| line.starts_with('╌')));
    assert!(!narrow.iter().any(|line| line.contains(['┌', '│', '└'])));

    // Dragging through the trailing header rule includes the header row; starting on it excludes it.
    let with_above = format!("Above\n\n{source}");
    let rows = render_detail(&with_above, 60);
    let range = (hit(&rows, "Above", false), hit(&rows, "────", true));
    assert_eq!(
        selection::text(&rows, range).as_deref(),
        Some("Above\n\n| Svc | Notes | Ms |\n| --- | --- | ---: |")
    );
    let below_rule = (hit(&rows, "────", false), hit(&rows, "12", true));
    assert_eq!(
        selection::text(&rows, below_rule).as_deref(),
        Some("| api | Validates JWT tokens and keys | 12 |")
    );

    // 21 1-char columns fit at width 80 (21 + 20*2 = 61 <= 80) without stacking.
    let cols = 21;
    let hdr = format!("|{}", " x |".repeat(cols));
    let sep = format!("|{}", " - |".repeat(cols));
    let body = format!("|{}", " y |".repeat(cols));
    let many = render_detail(&format!("{hdr}\n{sep}\n{body}"), 80);
    assert_eq!(many.len(), 3);

    // Cells wider than MAX_TABLE_COLUMNS (120) still wrap and get dashed body dividers.
    let long = "w".repeat(130);
    let overwide: Vec<_> = render_detail(&format!("| H |\n| --- |\n| {long} |\n| next |"), 140)
        .into_iter()
        .map(|row| row.line().to_string())
        .collect();
    assert!(overwide.iter().any(|line| line.starts_with('╌')));

    // An open streaming table after a closed fence and footnote ref keeps consistent widths.
    let start = "```txt\nhi\n```\n\nSee [^ref]\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n";
    let mut stream = Stream::default();
    stream.append(start);
    let mut committed = stream.flush();
    let delta = "| wide_col_a | wide_col_b |\n";
    stream.append(delta);
    committed.extend(stream.flush());
    let mut visible = committed.clone();
    visible.extend_from_slice(stream.live());
    committed.extend(stream.finish());
    let expected = separated_with(&format!("{start}{delta}"), &References::new());
    let project = |rows: &[RenderRow]| {
        selectable(rows, 0, 80)
            .into_iter()
            .map(Row::into_line)
            .collect::<Vec<_>>()
    };
    assert_eq!(project(&visible), project(&expected));
    assert_eq!(project(&committed), project(&expected));
}
