use super::*;
#[test]
fn terminal_state_is_restored_once_and_only_where_bart_set_it() {
    for (owned, pops) in [(RAW, 0), (RAW | TITLE | KEYS | SCREEN | MOUSE | FOCUS, 1)] {
        let owned = AtomicU8::new(owned);
        let mut output = Vec::new();
        restore_input(&mut output, &owned);
        restore_input(&mut output, &owned);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("\x1b[<1u").count(), pops);
        assert_eq!(output.matches("\x1b[23;2t").count(), pops);
        assert_eq!(output.matches("\x1b[?1004l").count(), pops);
        assert_eq!(output.matches("\x1b[?2004l").count(), 1);
    }
}
