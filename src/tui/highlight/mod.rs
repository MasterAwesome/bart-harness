use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::sync::LazyLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::theme;

#[derive(Clone, Copy)]
enum Kind {
    Comment,
    String,
    Literal,
    Keyword,
    Plain,
    Call,
}

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

static KINDS: LazyLock<Vec<(Scope, Kind)>> = LazyLock::new(|| {
    [
        ("comment", Kind::Comment),
        ("string", Kind::String),
        ("constant.numeric", Kind::Literal),
        ("constant.language", Kind::Literal),
        ("constant.character", Kind::Literal),
        ("entity.name.type", Kind::Literal),
        ("support.type", Kind::Literal),
        ("support.class", Kind::Literal),
        ("keyword.operator", Kind::Plain),
        ("keyword", Kind::Keyword),
        ("storage", Kind::Keyword),
        ("entity.name.function", Kind::Call),
        ("support.function", Kind::Call),
        ("variable.function", Kind::Call),
    ]
    .into_iter()
    .map(|(scope, kind)| (Scope::new(scope).expect("valid scope"), kind))
    .collect()
});

pub(super) fn python(code: &str) -> Vec<Line<'static>> {
    highlight(code, "python").unwrap_or_else(|| plain(code))
}

pub(super) fn highlight(code: &str, language: &str) -> Option<Vec<Line<'static>>> {
    let syntax = SYNTAXES.find_syntax_by_token(language)?;
    let theme = theme();
    let style = |kind: Kind| match kind {
        Kind::Comment => super::transcript::muted(Style::new().italic()),
        Kind::String => Style::new().fg(theme.success),
        Kind::Literal => Style::new().fg(theme.literal),
        Kind::Keyword => Style::new().fg(theme.keyword).bold(),
        Kind::Call => Style::new().fg(theme.code),
        Kind::Plain => Style::new(),
    };
    let current = |stack: &ScopeStack| {
        stack
            .as_slice()
            .iter()
            .rev()
            .find_map(|&scope| {
                KINDS
                    .iter()
                    .find(|(prefix, _)| prefix.is_prefix_of(scope))
                    .map(|&(_, kind)| style(kind))
            })
            .unwrap_or_default()
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for text in code.split('\n') {
        let line = format!("{text}\n");
        let mut spans = Vec::new();
        let mut at = 0;
        for (index, op) in state.parse_line(&line, &SYNTAXES).ok()? {
            let index = index.min(text.len());
            if index > at {
                spans.push(Span::styled(text[at..index].to_owned(), current(&stack)));
                at = index;
            }
            stack.apply(&op).ok()?;
        }
        if at < text.len() {
            spans.push(Span::styled(text[at..].to_owned(), current(&stack)));
        }
        lines.push(Line::from(spans));
    }
    Some(lines)
}

fn plain(code: &str) -> Vec<Line<'static>> {
    code.split('\n')
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

fn string_literal(text: &str) -> Option<usize> {
    let prefix = text
        .char_indices()
        .take(3)
        .find(|&(_, c)| c == '\'' || c == '"')
        .map(|(end, _)| end)
        .filter(|&end| text[..end].chars().all(|c| "rRbBfFuU".contains(c)))?;
    let body = &text[prefix..];
    let quote = &body[..1];
    let delimiter = if body.starts_with(&quote.repeat(3)) {
        quote.repeat(3)
    } else {
        quote.to_owned()
    };
    let mut at = delimiter.len();
    while at < body.len() {
        let rest = &body[at..];
        if rest.starts_with(&delimiter) {
            return Some(prefix + at + delimiter.len());
        }
        if rest.starts_with('\n') && delimiter.len() == 1 {
            return Some(prefix + at);
        }
        let step = if rest.starts_with('\\') { 2 } else { 1 };
        at += rest.chars().take(step).map(char::len_utf8).sum::<usize>();
    }
    Some(text.len())
}

const COMPOUND: [&str; 14] = [
    "if", "elif", "else", "for", "while", "with", "def", "class", "try", "except", "finally",
    "async", "match", "case",
];

pub(super) fn statements(code: &str) -> std::borrow::Cow<'_, str> {
    let mut cuts = Vec::new();
    let mut depth = 0usize;
    let mut indent = 0;
    let mut first: Option<&str> = None;
    let mut line_start = true;
    let mut at = 0;
    while at < code.len() {
        let rest = &code[at..];
        let c = rest.chars().next().expect("not at the end");
        if line_start && depth == 0 {
            indent = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            at += indent;
            line_start = false;
            continue;
        }
        let length = if c == '#' {
            rest.find('\n').unwrap_or(rest.len())
        } else if let Some(length) = string_literal(rest) {
            first.get_or_insert("");
            length
        } else if c.is_alphabetic() || c == '_' {
            let length = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            first.get_or_insert(&rest[..length]);
            length
        } else {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                '\\' if rest[1..].starts_with('\n') => {
                    at += 2;
                    continue;
                }
                '\n' if depth == 0 => {
                    first = None;
                    line_start = true;
                }
                ';' if depth == 0 && !first.is_some_and(|word| COMPOUND.contains(&word)) => {
                    cuts.push((at, indent));
                }
                c if !c.is_whitespace() => {
                    first.get_or_insert("");
                }
                _ => {}
            }
            c.len_utf8()
        };
        at += length;
    }
    if cuts.is_empty() {
        return code.into();
    }
    let mut split = code.to_owned();
    for (at, indent) in cuts.into_iter().rev() {
        let rest = split[at + 1..].trim_start_matches([' ', '\t']);
        let rest = if rest.is_empty() || rest.starts_with('\n') {
            rest.to_owned()
        } else {
            format!("\n{}{rest}", " ".repeat(indent))
        };
        split.truncate(split[..at].trim_end_matches([' ', '\t']).len());
        split.push_str(&rest);
    }
    split.into()
}
