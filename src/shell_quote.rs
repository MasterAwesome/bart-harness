use std::fmt::Write as _;

pub(crate) fn quote(value: &str) -> String {
    if !value.chars().any(crate::terminal_text::unsafe_character) {
        return format!("'{}'", value.replace('\'', "'\"'\"'"));
    }
    let mut quoted = String::from("$'");
    for c in value.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            c if crate::terminal_text::unsafe_character(c) => {
                let _ = write!(quoted, "\\u{:04x}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

pub(crate) fn argv(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
            {
                arg.clone()
            } else {
                quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
