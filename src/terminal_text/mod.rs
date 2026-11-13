use std::{
    collections::VecDeque,
    fmt,
    io::{self, IsTerminal as _, Write as _},
    str,
    sync::Mutex,
};

pub(crate) fn unsafe_character(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[derive(Clone, Copy)]
pub(crate) enum TextMode {
    Consent,
    Diagnostic,
    Diff,
}

pub(crate) fn safe_text(text: &str, mode: TextMode) -> String {
    let mut output = String::new();
    if matches!(mode, TextMode::Diagnostic) {
        Display::default().push(text, |part| output.push_str(part));
        return output
            .chars()
            .map(|c| if unsafe_character(c) { ' ' } else { c })
            .collect();
    }
    for c in text.chars() {
        match mode {
            TextMode::Consent if c == '\n' => output.push(c),
            TextMode::Consent if unsafe_character(c) => {
                use std::fmt::Write as _;
                let _ = write!(output, "{}", c.escape_unicode());
            }
            TextMode::Diff if c == '\t' => output.push_str("    "),
            TextMode::Diff if c == '\n' => output.push(c),
            TextMode::Diff if unsafe_character(c) => output.push(char::REPLACEMENT_CHARACTER),
            _ => output.push(c),
        }
    }
    output
}

pub(crate) fn io_reason(error: &io::Error) -> String {
    let mut text = error.to_string();
    if let Some(code) = error.raw_os_error() {
        let suffix = format!(" (os error {code})");
        if let Some(stripped) = text.strip_suffix(&suffix) {
            text.truncate(stripped.len());
        }
    }
    text
}

const WRAP_COLUMNS: usize = 16 * 1024;
const MAX_CSI_PARAM_BYTES: usize = 32;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    CsiIgnore,
    Osc,
    IgnoredString,
    StringEscape,
}

#[derive(Default)]
enum Rendering {
    #[default]
    Command,
    Display,
}

#[derive(Default)]
pub(crate) struct TerminalText {
    rendering: Rendering,
    line: Vec<char>,
    column: usize,
    state: State,
    params: String,
    csi_intermediate: bool,
    utf8: Vec<u8>,
    text: String,
}

impl TerminalText {
    pub(crate) fn current_line_bytes(&self) -> usize {
        self.line.iter().map(|character| character.len_utf8()).sum()
    }

    pub(crate) fn current_line(&self) -> String {
        self.line.iter().collect()
    }

    pub(crate) fn push(&mut self, bytes: &[u8], mut emit: impl FnMut(&str)) {
        self.feed(bytes, &mut emit);
    }

    fn feed<F: FnMut(&str)>(&mut self, mut bytes: &[u8], emit: &mut F) {
        while !self.utf8.is_empty()
            && let Some((&byte, rest)) = bytes.split_first()
        {
            bytes = rest;
            self.utf8.push(byte);
            match str::from_utf8(&self.utf8) {
                Ok(c) => {
                    let c = c.chars().next().unwrap_or(char::REPLACEMENT_CHARACTER);
                    self.utf8.clear();
                    self.input(c, emit);
                }
                Err(error) => {
                    if let Some(invalid) = error.error_len() {
                        let rest = self.utf8.split_off(invalid);
                        self.utf8.clear();
                        self.input(char::REPLACEMENT_CHARACTER, emit);
                        self.feed(&rest, emit);
                    }
                }
            }
        }
        let mut chunks = bytes.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            for c in chunk.valid().chars() {
                self.input(c, emit);
            }
            let invalid = chunk.invalid();
            if chunks.peek().is_none()
                && str::from_utf8(invalid).is_err_and(|e| e.error_len().is_none())
            {
                self.utf8.extend_from_slice(invalid);
            } else if !invalid.is_empty() {
                self.input(char::REPLACEMENT_CHARACTER, emit);
            }
        }
    }

    pub(crate) fn finish(mut self, mut emit: impl FnMut(&str)) {
        if !self.utf8.is_empty() {
            self.input(char::REPLACEMENT_CHARACTER, &mut emit);
        }
        if self.line.iter().any(|c| !c.is_whitespace()) {
            self.commit(&mut emit);
        }
    }

    fn input(&mut self, c: char, emit: &mut impl FnMut(&str)) {
        if unsafe_character(c) && !c.is_control() {
            return;
        }
        if matches!(self.rendering, Rendering::Display) {
            match c {
                '\u{009b}' | '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                    self.state = match c {
                        '\u{009b}' => State::Csi,
                        '\u{009d}' => State::Osc,
                        _ => State::IgnoredString,
                    };
                    return;
                }
                '\u{009c}' => {
                    self.state = State::Ground;
                    return;
                }
                '\x1b'
                    if matches!(
                        self.state,
                        State::Osc | State::IgnoredString | State::StringEscape
                    ) =>
                {
                    self.state = State::StringEscape;
                    return;
                }
                _ if matches!(self.state, State::StringEscape) => {
                    self.state = if c == '\\' {
                        State::Ground
                    } else {
                        State::IgnoredString
                    };
                    return;
                }
                _ => {}
            }
        }
        match c {
            '\x18' | '\x1a' => self.state = State::Ground,
            '\x1b' => self.state = State::Escape,
            '\x7f' => {}
            '\x07' if matches!(self.state, State::Osc) => self.state = State::Ground,
            _ if matches!(self.state, State::Osc | State::IgnoredString) => {}
            '\0'..='\x1f' => self.execute(c, emit),
            _ => match self.state {
                State::Ground if !unsafe_character(c) => self.print(c, emit),
                State::Ground | State::Osc | State::IgnoredString | State::StringEscape => {}
                State::Escape => {
                    self.state = match c {
                        '[' => {
                            self.params.clear();
                            self.csi_intermediate = false;
                            State::Csi
                        }
                        ']' => State::Osc,
                        'P' | 'X' | '^' | '_' => State::IgnoredString,
                        ' '..='/' => State::EscapeIntermediate,
                        _ => State::Ground,
                    };
                }
                State::EscapeIntermediate => {
                    if !(' '..='/').contains(&c) {
                        self.state = State::Ground;
                    }
                }
                State::Csi => match c {
                    '0'..='?'
                        if !self.csi_intermediate && self.params.len() < MAX_CSI_PARAM_BYTES =>
                    {
                        self.params.push(c);
                    }
                    ' '..='/' => self.csi_intermediate = true,
                    '@'..='~' => {
                        self.state = State::Ground;
                        if !self.csi_intermediate {
                            self.dispatch_csi(c);
                        }
                    }
                    _ => self.state = State::CsiIgnore,
                },
                State::CsiIgnore => {
                    if ('@'..='~').contains(&c) {
                        self.state = State::Ground;
                    }
                }
            },
        }
    }

    fn execute(&mut self, c: char, emit: &mut impl FnMut(&str)) {
        if matches!(self.rendering, Rendering::Display) {
            if matches!(self.state, State::Ground) && matches!(c, '\n' | '\t') {
                emit(c.encode_utf8(&mut [0; 4]));
            }
            return;
        }
        match c {
            '\n' => self.commit(emit),
            '\r' => self.column = 0,
            '\x08' => self.column = self.column.saturating_sub(1),
            '\t' => self.print('\t', emit),
            _ => {}
        }
    }

    fn dispatch_csi(&mut self, command: char) {
        if matches!(self.rendering, Rendering::Display) {
            return;
        }
        if self.params.starts_with(['<', '=', '>', '?']) {
            return;
        }
        let first = self.params.split([';', ':']).next().unwrap_or_default();
        let n: usize = first.parse().unwrap_or(0);
        let last_column = WRAP_COLUMNS - 1;
        match command {
            'G' => self.column = (n.max(1) - 1).min(last_column),
            'C' => self.column = self.column.saturating_add(n.max(1)).min(last_column),
            'D' => self.column = self.column.saturating_sub(n.max(1)),
            'K' => match n {
                0 => self.line.truncate(self.column),
                1 => {
                    let end = (self.column + 1).min(self.line.len());
                    self.line[..end].fill(' ');
                }
                2 => self.line.clear(),
                _ => {}
            },
            _ => {}
        }
    }

    fn print(&mut self, c: char, emit: &mut impl FnMut(&str)) {
        if matches!(self.rendering, Rendering::Display) {
            emit(c.encode_utf8(&mut [0; 4]));
            return;
        }
        if self.column == WRAP_COLUMNS {
            self.commit(emit);
        }
        if let Some(cell) = self.line.get_mut(self.column) {
            *cell = c;
        } else {
            self.line.resize(self.column, ' ');
            self.line.push(c);
        }
        self.column += 1;
    }

    fn commit(&mut self, emit: &mut impl FnMut(&str)) {
        let end = self
            .line
            .iter()
            .rposition(|c| !c.is_whitespace())
            .map_or(0, |i| i + 1);
        self.text.clear();
        self.text.extend(&self.line[..end]);
        emit(&self.text);
        self.line.clear();
        self.column = 0;
    }
}

pub(crate) enum Display {
    Raw,
    Terminal(TerminalText),
}

impl Default for Display {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Display {
    pub(crate) fn new(terminal: bool) -> Self {
        if terminal {
            Self::Terminal(TerminalText {
                rendering: Rendering::Display,
                ..TerminalText::default()
            })
        } else {
            Self::Raw
        }
    }

    pub(crate) fn push(&mut self, text: &str, mut emit: impl FnMut(&str)) {
        match self {
            Self::Raw => emit(text),
            Self::Terminal(filter) => {
                let mut displayed = String::with_capacity(text.len());
                filter.push(text.as_bytes(), |part| displayed.push_str(part));
                emit(&displayed);
            }
        }
    }
}

pub(crate) fn stdout(args: fmt::Arguments<'_>) {
    let mut output = io::stdout().lock();
    Display::new(output.is_terminal()).push(&args.to_string(), |text| {
        let _ = output.write_all(text.as_bytes());
    });
    let _ = output.flush();
}

pub(crate) fn stderr(args: fmt::Arguments<'_>) {
    let mut output = io::stderr().lock();
    Display::new(output.is_terminal()).push(&args.to_string(), |text| {
        let _ = output.write_all(text.as_bytes());
    });
}

static LINES: Mutex<Option<VecDeque<String>>> = Mutex::new(None);
static LINE_READY: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub(crate) fn install_sink() {
    *LINES.lock().expect(crate::NOT_POISONED) = Some(VecDeque::new());
}

pub(crate) fn remove_sink() {
    let mut sink = LINES.lock().expect(crate::NOT_POISONED);
    if let Some(lines) = sink.take() {
        for line in lines {
            stderr(format_args!("{line}\n"));
        }
    }
}

pub(crate) async fn line_ready() {
    LINE_READY.notified().await;
}

pub(crate) fn pending_line() -> Option<String> {
    LINES
        .lock()
        .expect(crate::NOT_POISONED)
        .as_ref()?
        .front()
        .cloned()
}

pub(crate) fn line_shown() {
    if let Some(lines) = LINES.lock().expect(crate::NOT_POISONED).as_mut() {
        lines.pop_front();
        if !lines.is_empty() {
            LINE_READY.notify_one();
        }
    }
}

fn line(args: fmt::Arguments<'_>, stdout: bool) {
    let mut sink = LINES.lock().expect(crate::NOT_POISONED);
    if let Some(lines) = sink.as_mut() {
        lines.push_back(args.to_string());
        LINE_READY.notify_one();
    } else if stdout {
        self::stdout(format_args!("{args}\n"));
    } else {
        stderr(format_args!("{args}\n"));
    }
}

pub(crate) fn stdout_line(args: fmt::Arguments<'_>) {
    line(args, true);
}

pub(crate) fn stderr_line(args: fmt::Arguments<'_>) {
    line(args, false);
}

#[cfg(test)]
mod tests;
