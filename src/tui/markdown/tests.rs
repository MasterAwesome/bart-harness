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
