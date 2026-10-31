use super::*;

#[test]
fn display_removes_controls_at_every_byte_split() {
    let cases = [
        ("a\x1b[31mb\x1b[0mc", "abc"),
        ("a\x1b]52;c;Y2xpcA==\x07b", "ab"),
        ("a\x1b]0;title\x1b\\b", "ab"),
        ("a\x1bPpayload\x1b\\b", "ab"),
        (
            "a\x1bXpayload\x1b\\b\x1b^secret\x1b\\c\x1b_hidden\x1b\\d",
            "abcd",
        ),
        (
            "a\u{9b}31mb\u{9d}52;c;clip\u{9c}c\u{90}payload\u{9c}d",
            "abcd",
        ),
        (
            "a\u{98}payload\u{9c}b\u{9e}payload\u{9c}c\u{9f}payload\u{9c}d",
            "abcd",
        ),
        ("a\x1b(Bb\x1b7c\x1b", "abc"),
        ("a\r\x08\x07\0\x7fb\n\t", "ab\n\t"),
        (
            "a\u{61c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}b",
            "ab",
        ),
        ("a\x1b]secret\x1b[31mstill secret\x1b\\b", "ab"),
        ("a\x1b[1;2\x18b\x1b]unfinished", "ab"),
        (
            "日本語 café e\u{301} العربية 👩‍💻\n\t end ",
            "日本語 café e\u{301} العربية 👩‍💻\n\t end ",
        ),
    ];
    for (input, expected) in cases {
        let bytes = input.as_bytes();
        for split in 0..=bytes.len() {
            let mut filter = TerminalText {
                rendering: Rendering::Display,
                ..TerminalText::default()
            };
            let mut output = String::new();
            filter.push(&bytes[..split], |text| output.push_str(text));
            filter.push(&bytes[split..], |text| output.push_str(text));
            assert_eq!(output, expected, "{input:?}, split {split}");
        }
    }
    let controls: String = ('\u{80}'..='\u{9f}')
        .filter(|c| {
            !matches!(
                c,
                '\u{90}' | '\u{98}' | '\u{9b}' | '\u{9d}' | '\u{9e}' | '\u{9f}'
            )
        })
        .collect();
    let mut output = String::new();
    Display::default().push(&format!("a{controls}b"), |text| output.push_str(text));
    assert_eq!(output, "ab");
    for c in [
        '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
        '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ] {
        let input = format!("a{c}b");
        assert_eq!(safe_text(&input, TextMode::Diagnostic), "ab");
        let consent = safe_text(&input, TextMode::Consent);
        assert!(consent.starts_with('a') && consent.ends_with('b'));
        assert!(!consent.chars().any(unsafe_character));
        assert_ne!(consent, "ab", "consent must expose the hidden control");
        let mut displayed = String::new();
        Display::default().push(&consent, |text| displayed.push_str(text));
        assert_eq!(displayed, consent);
    }
}

fn render(chunks: &[&[u8]]) -> Vec<String> {
    let mut terminal = TerminalText::default();
    let mut lines = Vec::new();
    for chunk in chunks {
        terminal.push(chunk, |line| lines.push(line.to_owned()));
    }
    terminal.finish(|line| lines.push(line.to_owned()));
    lines
}

#[test]
fn renders_what_a_terminal_shows_under_every_chunking() {
    let input: &[u8] = b"Downloading  10%\rDownloading  55%\rDownloading 100%\n\
            abcdef\rXY\n\
            abcdef\rXY\x1b[K\n\
            abc\rX\x1b[?K\n\
            \x1b[2K\x1b[1Gstep 1/3\x1b[2K\x1b[1Gstep 3/3\n\
            \x1b[31mred\x1b[0m \x1b[1;38;2;1;2;3mbold\x1b[m\n\
            \x1b]0;title\x07pre \x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\\n\
            \x1b[?25lshown\x1b[?25h\x1bPq#0;2;0\x1b\\\x1b(B\n\
            a\x08b\tc\x00\x07\xC2\x9Bd\x1b[5Ce\x1b[3Df\n\
            x\x1b[1;2\x18y\n\
            caf\xC3\xA9 \xFF\xE2\x82\n\
            \xE2\xC3\xA9x\n\
            \n\
            50%\r";
    let expected = [
        "Downloading 100%",
        "XYcdef",
        "XY",
        "Xbc",
        "step 3/3",
        "red bold",
        "pre link",
        "shown",
        "b\tcd   f e",
        "xy",
        "café \u{FFFD}\u{FFFD}",
        "\u{FFFD}éx",
        "",
        "50%",
    ];
    for size in 1..=input.len() {
        let chunks: Vec<_> = input.chunks(size).collect();
        assert_eq!(render(&chunks), expected, "size {size}");
    }
    for first in 0..=input.len() {
        for second in first..=input.len() {
            let chunks = [&input[..first], &input[first..second], &input[second..]];
            assert_eq!(render(&chunks), expected, "cuts {first}, {second}");
        }
    }
}
