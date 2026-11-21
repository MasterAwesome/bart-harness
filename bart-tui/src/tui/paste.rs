use crate::tui::text::Clean;
use std::{borrow::Cow, fmt::Write as _, ops::Range, path::Path};

use super::MAX_DRAFT_BYTES;
use crate::tool::thousands;

const PREFIX: &str = "[Attached; ";
pub(super) const FOLD_BYTES: usize = 2048;

pub(super) fn folds(text: &str) -> bool {
    text.len() >= FOLD_BYTES || text.lines().count() > 10
}

#[derive(Clone)]
pub(super) struct Reference {
    pub(super) id: usize,
    pub(super) chars: usize,
}

pub(super) fn normalized_bytes(text: &str) -> usize {
    text.len()
        - text
            .as_bytes()
            .windows(2)
            .filter(|pair| *pair == b"\r\n")
            .count()
}

pub(super) fn normalize(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

pub(super) fn accepts(text: &str, current: usize, limit: usize) -> bool {
    current.checked_add(text.len()).is_some_and(|n| n <= limit)
        && !text.chars().any(crate::terminal_text::unsafe_character)
}

fn line_unit(lines: usize) -> &'static str {
    if lines == 1 { "line" } else { "lines" }
}

impl Reference {
    pub(super) fn text(&self) -> String {
        format!(
            "{PREFIX}{} chars; paste[{}]]",
            thousands(self.chars),
            self.id
        )
    }

    pub(super) fn file(&self, dir: &Path) -> std::path::PathBuf {
        dir.join(format!("paste-{}.txt", self.id))
    }
}

pub(super) fn references(text: &str) -> Vec<(Range<usize>, Reference)> {
    let mut found = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find(PREFIX) {
        let start = offset + start;
        offset = start + PREFIX.len();
        if let Some((length, reference)) = parse(&text[offset..]) {
            let mut end = offset + length;
            if text[end..].starts_with('\n') {
                end += 1;
            }
            found.push((start..end, reference));
            offset = end;
        }
    }
    found
}

fn parse(text: &str) -> Option<(usize, Reference)> {
    let close = text.find("]]")?;
    let (chars, id) = text[..close].split_once(" chars; paste[")?;
    let chars = chars.replace(',', "").parse().ok()?;
    let id = id.parse().ok()?;
    (id > 0 && chars <= MAX_DRAFT_BYTES).then_some((close + 2, Reference { id, chars }))
}

pub(super) fn content_bytes(text: &str) -> usize {
    text.len()
        + references(text)
            .iter()
            .map(|(_, reference)| reference.chars)
            .sum::<usize>()
}

pub(super) type Previews = std::collections::HashMap<usize, Clean>;

pub(super) fn previews(dir: &Path) -> std::io::Result<Previews> {
    let mut previews = Previews::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| {
            name.strip_prefix("paste-")?
                .strip_suffix(".txt")?
                .parse()
                .ok()
        }) {
            previews.insert(id, load_preview(dir, id));
        }
    }
    Ok(previews)
}

pub(super) fn load_preview(dir: &Path, id: usize) -> Clean {
    let reference = Reference { id, chars: 0 };
    let content = crate::persist::open(&reference.file(dir))
        .and_then(|file| crate::persist::read_to(&file, MAX_DRAFT_BYTES as u64))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    preview(id, &content)
}

pub(super) fn preview(id: usize, content: &str) -> Clean {
    let lines: Vec<_> = content.lines().collect();
    let mut shown = format!("[Pasted #{id} +{} {}]", lines.len(), line_unit(lines.len()));
    for (index, line) in lines.iter().enumerate() {
        if lines.len() > 10 && (5..lines.len() - 5).contains(&index) {
            if index == 5 {
                let _ = write!(shown, "\n… {} more lines …", lines.len() - 10);
            }
            continue;
        }
        shown.push('\n');
        shown.push_str(&super::text::clip_end(&Clean::new(line), 76));
    }
    Clean::new(&shown)
}

pub(super) fn display(text: &str, previews: &Previews) -> Vec<(String, bool)> {
    let mut parts = Vec::new();
    let mut end = 0;
    for (range, reference) in references(text) {
        parts.push((text[end..range.start].to_owned(), false));
        let mut shown = previews
            .get(&reference.id)
            .cloned()
            .unwrap_or_else(|| preview(reference.id, ""))
            .to_string();
        if range.end < text.len() {
            shown.push('\n');
        }
        parts.push((shown, true));
        end = range.end;
    }
    parts.push((text[end..].to_owned(), false));
    parts
}

pub(super) fn composer_token(id: usize, content: &str) -> String {
    let lines = content.lines().count();
    format!("[Pasted #{id} +{lines} {}]", line_unit(lines))
}
