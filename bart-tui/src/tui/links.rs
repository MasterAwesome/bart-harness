use ratatui::{
    buffer::{Buffer, CellDiffOption, CellWidth},
    style::{Color, Modifier, Style},
    text::Span,
};
use reqwest::Url;
use std::{
    collections::HashMap,
    env,
    num::NonZeroU16,
    path::Path,
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::Duration,
};

const MAX_URL: usize = 2048;
const MAX_TARGETS: usize = u16::MAX as usize;
const MAX_TARGET_BYTES: usize = 4 * 1024 * 1024;
const MARKER_TAG: u8 = 0xba;

static ENABLED: OnceLock<bool> = OnceLock::new();
static TARGETS: LazyLock<Mutex<Targets>> = LazyLock::new(Default::default);

#[cfg(test)]
std::thread_local! {
    static TEST_ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[derive(Default)]
struct Targets {
    urls: Vec<Arc<str>>,
    ids: HashMap<Arc<str>, u16>,
    bytes: usize,
}

impl Targets {
    fn intern(&mut self, url: &str) -> Option<u16> {
        if let Some(&id) = self.ids.get(url) {
            return Some(id);
        }
        if self.urls.len() >= MAX_TARGETS || self.bytes.saturating_add(url.len()) > MAX_TARGET_BYTES
        {
            return None;
        }
        let id = u16::try_from(self.urls.len()).ok()?;
        let url: Arc<str> = Arc::from(url);
        self.bytes += url.len();
        self.urls.push(Arc::clone(&url));
        self.ids.insert(url, id);
        Some(id)
    }

    fn get(&self, id: u16) -> Option<&str> {
        self.urls.get(usize::from(id)).map(Arc::as_ref)
    }
}

pub(super) async fn init() {
    let _ = ENABLED.set(detect().await);
}

async fn detect() -> bool {
    if env::var_os("FORCE_HYPERLINK").is_some() {
        return env::var("FORCE_HYPERLINK").is_ok_and(|value| value.trim() != "0");
    }
    if env::var_os("TMUX").is_some() {
        let Some(pane) = env::var_os("TMUX_PANE") else {
            return false;
        };
        let mut command = tokio::process::Command::new("tmux");
        command
            .args(["list-clients", "-t"])
            .arg(pane)
            .args(["-F", "#{client_termtype}\t#{client_termfeatures}"])
            .kill_on_drop(true);
        let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(1), command.output()).await
        else {
            return false;
        };
        let clients = String::from_utf8_lossy(&output.stdout);
        return output.status.success() && tmux_clients_support_hyperlinks(&clients);
    }
    env_supports_hyperlinks(|name| env::var(name).ok())
}

fn tmux_clients_support_hyperlinks(clients: &str) -> bool {
    !clients.is_empty()
        && clients.lines().all(|line| {
            line.split_once('\t').is_some_and(|(kind, features)| {
                !kind.starts_with("XTerm(") && features.split(',').any(|f| f == "hyperlinks")
            })
        })
}

fn env_supports_hyperlinks(var: impl Fn(&str) -> Option<String>) -> bool {
    if var("STY").is_some() {
        return false;
    }
    let term = var("TERM").unwrap_or_default();
    if term == "dumb"
        || term.starts_with("screen")
        || (matches!(term.as_str(), "xterm" | "xterm-256color") && var("XTERM_VERSION").is_some())
    {
        return false;
    }
    if var("DOMTERM").is_some() || var("WT_SESSION").is_some() || var("KONSOLE_VERSION").is_some() {
        return true;
    }
    if var("VTE_VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 5000)
    {
        return true;
    }
    if var("TERM_PROGRAM").is_some_and(|program| {
        matches!(
            program.as_str(),
            "Hyper" | "iTerm.app" | "terminology" | "WezTerm" | "vscode" | "ghostty" | "zed"
        )
    }) {
        return true;
    }
    if matches!(
        term.as_str(),
        "xterm-kitty"
            | "alacritty"
            | "alacritty-direct"
            | "foot"
            | "foot-extra"
            | "wezterm"
            | "xterm-ghostty"
    ) {
        return true;
    }
    var("COLORTERM").is_some_and(|value| value == "xfce4-terminal")
}

fn enabled() -> bool {
    #[cfg(test)]
    if let Some(value) = TEST_ENABLED.with(std::cell::Cell::get) {
        return value;
    }
    ENABLED.get().copied().unwrap_or(false)
}

#[cfg(test)]
pub(super) fn with_enabled<T>(value: bool, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_ENABLED.with(|cell| cell.set(self.0));
        }
    }
    let previous = TEST_ENABLED.with(|cell| cell.replace(Some(value)));
    let _reset = Reset(previous);
    f()
}

pub(super) fn file(path: &Path) -> Option<Url> {
    Url::from_file_path(std::path::absolute(path).ok()?).ok()
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && value.parse::<u32>().is_ok()
}

fn strip_location(text: &str) -> Option<&str> {
    let (head, last) = text.rsplit_once(':')?;
    if !is_digits(last) {
        return None;
    }
    if let Some((path, line)) = head.rsplit_once(':')
        && is_digits(line)
        && Path::new(path).is_file()
    {
        return Some(path);
    }
    Path::new(head).is_file().then_some(head)
}

fn existing_path(path: &Path) -> bool {
    path.is_file() || (path.file_name().is_some() && path.is_dir())
}

fn destination(text: &str) -> Option<Url> {
    if text.is_empty()
        || text.len() > MAX_URL
        || text.chars().any(char::is_control)
        || text.starts_with("//")
    {
        return None;
    }
    if text.contains("://") {
        let url = Url::parse(text).ok()?;
        return match url.scheme() {
            "https" | "http" if url.has_host() => Some(url),
            "file" => file(&url.to_file_path().ok()?),
            _ => None,
        };
    }
    if let Some(path) = strip_location(text) {
        return file(Path::new(path));
    }
    let path = Path::new(text);
    existing_path(path).then(|| file(path)).flatten()
}

fn markdown_target(text: &str) -> Option<Url> {
    if text.is_empty()
        || text.len() > MAX_URL
        || text.chars().any(char::is_control)
        || text.starts_with('#')
    {
        return None;
    }
    if text.contains(['#', '%']) && !text.contains("://") && !text.starts_with("//") {
        let base = Url::from_directory_path(std::env::current_dir().ok()?).ok()?;
        let path = base.join(text).ok()?.to_file_path().ok()?;
        if path.is_file() {
            return file(&path);
        }
    }
    destination(text)
}

pub(super) fn markdown_style(style: Style, destination: &str) -> Option<Style> {
    if !enabled() {
        return None;
    }
    register(style, &markdown_target(destination)?)
}

pub(super) fn style(style: Style, url: Option<Url>) -> Style {
    if !enabled() {
        return style;
    }
    url.and_then(|url| register(style, &url)).unwrap_or(style)
}

fn register(style: Style, url: &Url) -> Option<Style> {
    if !matches!(url.scheme(), "http" | "https" | "file")
        || url.as_str().len() > MAX_URL
        || url.as_str().chars().any(char::is_control)
    {
        return None;
    }
    let mut targets = TARGETS.lock().expect(crate::NOT_POISONED);
    let [hi, lo] = targets.intern(url.as_str())?.to_be_bytes();
    Some(style.underline_color(Color::Rgb(MARKER_TAG, hi, lo)))
}

fn trim_token(mut token: &str) -> &str {
    loop {
        let before = token.len();
        token = token.trim_end_matches(['.', ',', ';', ':']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            if token.ends_with(close) {
                let excess = token
                    .matches(close)
                    .count()
                    .saturating_sub(token.matches(open).count());
                let trailing = token.len() - token.trim_end_matches(close).len();
                token = &token[..token.len() - excess.min(trailing)];
            }
        }
        if token.len() == before {
            return token;
        }
    }
}

pub(super) fn annotated(spans: &[Span<'_>]) -> Option<Vec<Span<'static>>> {
    if !enabled()
        || !spans
            .iter()
            .any(|s| s.style.underline_color.is_none() && s.content.contains(['/', '.']))
    {
        return None;
    }
    let source;
    let text = if let [span] = spans {
        span.content.as_ref()
    } else {
        source = spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        &source
    };
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(c) = text[cursor..].chars().next() {
        if c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '>' | '=') {
            cursor += c.len_utf8();
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            cursor += 1;
            let rest = &text[cursor..];
            if let Some(end) =
                rest[..rest.floor_char_boundary(rest.len().min(MAX_URL + 1))].find([c, '\n'])
                && rest[end..].starts_with(c)
                && rest[..end].contains(['/', '.'])
                && let Some(url) = destination(&rest[..end])
                && let Some(marker) = register(Style::new(), &url)
            {
                links.push((cursor..cursor + end, marker));
                cursor += end + 1;
            }
            continue;
        }
        let start = cursor;
        let rest = &text[start..];
        cursor += rest
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | '<' | '>'))
            .unwrap_or(rest.len());
        let token = trim_token(&text[start..cursor]);
        if token.contains(['/', '.'])
            && !token.contains('…')
            && let Some(url) = destination(token)
            && let Some(marker) = register(Style::new(), &url)
        {
            links.push((start..start + token.len(), marker));
        }
    }
    if links.is_empty() {
        return None;
    }
    let mut result = Vec::new();
    let mut links = links.into_iter().peekable();
    let mut base = 0;
    for span in spans {
        let end = base + span.content.len();
        let mut at = base;
        while at < end {
            while links.peek().is_some_and(|(range, _)| range.end <= at) {
                links.next();
            }
            let (to, marker) = match links.peek() {
                Some((range, marker)) if range.contains(&at) => (end.min(range.end), *marker),
                Some((range, _)) => (end.min(range.start), Style::new()),
                None => (end, Style::new()),
            };
            result.push(Span::styled(
                span.content[at - base..to - base].to_owned(),
                marker.patch(span.style),
            ));
            at = to;
        }
        base = end;
    }
    Some(result)
}

pub(super) fn clipped(text: &str, visible: String, base: Style) -> Vec<Span<'static>> {
    if visible.is_empty() {
        return Vec::new();
    }
    let Some(mut spans) = annotated(&[Span::styled(text, base)]) else {
        return vec![Span::styled(visible, base)];
    };
    if text == visible {
        return spans;
    }
    let Some(prefix) = visible.strip_suffix('…') else {
        return vec![Span::styled(visible, base)];
    };
    if !text.starts_with(prefix) {
        return vec![Span::styled(visible, base)];
    }
    let mut left = prefix.len();
    for (index, span) in spans.iter_mut().enumerate() {
        if left <= span.content.len() {
            if left == 0 {
                spans.truncate(index);
                spans.push(Span::styled("…", base));
            } else if left == span.content.len() {
                spans.truncate(index + 1);
                spans.push(Span::styled("…", base));
            } else {
                let value = span.content.to_mut();
                value.truncate(left);
                value.push('…');
                spans.truncate(index + 1);
            }
            return spans;
        }
        left -= span.content.len();
    }
    spans.push(Span::styled("…", base));
    spans
}

pub(super) fn paint(buf: &mut Buffer) {
    if !enabled() {
        return;
    }
    let targets = TARGETS.lock().expect(crate::NOT_POISONED);
    for cell in &mut buf.content {
        let Color::Rgb(MARKER_TAG, hi, lo) = cell.underline_color else {
            continue;
        };
        cell.underline_color = Color::Reset;
        let id = u16::from_be_bytes([hi, lo]);
        if let Some(url) = targets.get(id)
            && let Some(width) = NonZeroU16::new(cell.cell_width())
        {
            let symbol = format!(
                "\x1b]8;id=bart{id};{url}\x1b\\{}\x1b]8;;\x1b\\",
                cell.symbol()
            );
            cell.modifier.insert(Modifier::UNDERLINED);
            cell.set_symbol(&symbol)
                .set_diff_option(CellDiffOption::ForcedWidth(width));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, text::Line};

    #[test]
    fn detection_handles_terminals_tmux_and_screen() {
        assert!(env_supports_hyperlinks(|name| match name {
            "TERM" => Some("xterm-kitty".into()),
            _ => None,
        }));
        assert!(env_supports_hyperlinks(|name| match name {
            "TERM" => Some("xterm-256color".into()),
            "TERM_PROGRAM" => Some("ghostty".into()),
            _ => None,
        }));
        assert!(!env_supports_hyperlinks(|name| match name {
            "TERM" => Some("xterm-256color".into()),
            "TERM_PROGRAM" => Some("ghostty".into()),
            "STY" => Some("1234.pts".into()),
            _ => None,
        }));
        assert!(!env_supports_hyperlinks(|name| match name {
            "TERM" => Some("screen-256color".into()),
            "TERM_PROGRAM" => Some("iTerm.app".into()),
            _ => None,
        }));
        assert!(!env_supports_hyperlinks(|name| match name {
            "TERM" => Some("xterm-256color".into()),
            "XTERM_VERSION" => Some("XTerm(388)".into()),
            "VTE_VERSION" => Some("7000".into()),
            _ => None,
        }));
        assert!(tmux_clients_support_hyperlinks(
            "ghostty\tRGB,hyperlinks,sync\n"
        ));
        assert!(!tmux_clients_support_hyperlinks(
            "XTerm(388)\tRGB,hyperlinks\n"
        ));
        assert!(!tmux_clients_support_hyperlinks(""));
    }

    #[test]
    fn destination_rejects_nonexistent_slash_tokens_and_supports_locations() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let expected = file(&file_path).unwrap();

        for fake in [
            "/",
            "/*",
            "//comment",
            "/model",
            "/help",
            "/api/v1/users",
            "/definitely-does-not-exist-bart-test:42",
            ".",
            "..",
        ] {
            assert!(
                destination(fake).is_none(),
                "unexpected link for non-path token {fake:?}"
            );
        }

        assert_eq!(
            destination(&format!("{}:12", file_path.display())),
            Some(expected.clone())
        );
        assert_eq!(
            destination(&format!("{}:12:4", file_path.display())),
            Some(expected.clone())
        );
        assert_eq!(
            destination(&file_path.display().to_string()),
            Some(expected)
        );
        assert_eq!(
            destination(&dir.path().display().to_string()),
            file(dir.path())
        );
        assert_eq!(
            destination("https://example.com/docs?a=1"),
            Some(Url::parse("https://example.com/docs?a=1").unwrap())
        );
        assert!(destination("javascript://alert(1)").is_none());
        assert!(markdown_target("#overview").is_none());
    }

    #[test]
    fn trim_token_strips_nested_delimiters_and_punctuation() {
        assert_eq!(trim_token("https://example.com)]"), "https://example.com");
        assert_eq!(trim_token("https://example.com.)"), "https://example.com");
        assert_eq!(
            trim_token("https://en.wikipedia.org/wiki/Foo_(bar))"),
            "https://en.wikipedia.org/wiki/Foo_(bar)"
        );
        assert_eq!(trim_token("https://example.com},"), "https://example.com");
    }

    #[test]
    fn annotated_and_clipped_handle_boundaries_and_paint_osc8() {
        with_enabled(true, || {
            let line = "See [(https://example.com/a)] and (https://example.com/b.) and /model";
            let spans = annotated(&[Span::raw(line)]).expect("two URLs should be linked");
            let linked: Vec<_> = spans
                .iter()
                .filter(|s| s.style.underline_color.is_some())
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(
                linked,
                vec!["https://example.com/a", "https://example.com/b"]
            );

            let base = Style::new();
            let full = "open('https://example.com/long')";
            let before_link = clipped(full, "open('…".into(), base);
            assert_eq!(
                before_link
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                "open('…"
            );
            assert!(
                before_link
                    .iter()
                    .all(|s| s.style.underline_color.is_none()),
                "ellipsis must not inherit link style for completely clipped link"
            );

            let inside_link = clipped(full, "open('https://ex…".into(), base);
            assert_eq!(inside_link.len(), 2);
            assert_eq!(inside_link[0].content, "open('");
            assert!(inside_link[0].style.underline_color.is_none());
            assert_eq!(inside_link[1].content, "https://ex…");
            assert!(inside_link[1].style.underline_color.is_some());

            let after_link = clipped(full, "open('https://example.com/long…".into(), base);
            assert_eq!(after_link.last().unwrap().content, "…");
            assert!(after_link.last().unwrap().style.underline_color.is_none());

            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
            buf.set_line(0, 0, &Line::from(inside_link), 20);
            paint(&mut buf);
            assert_eq!(buf[(0, 0)].symbol(), "o");
            assert!(!buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));
            let cell = &buf[(6, 0)];
            assert!(cell.modifier.contains(Modifier::UNDERLINED));
            assert_eq!(cell.underline_color, Color::Reset);
            assert!(cell.symbol().starts_with("\x1b]8;id=bart"));
            assert!(
                cell.symbol()
                    .contains(";https://example.com/long\x1b\\h\x1b]8;;\x1b\\")
            );
            assert_eq!(cell.cell_width(), 1);
        });
    }
}
