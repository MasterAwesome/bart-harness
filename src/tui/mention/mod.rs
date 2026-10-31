use crate::tui::text::Clean;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const MAX_FILES: usize = 200_000;
pub(super) const SHOWN: usize = 20;

pub(super) struct Index {
    paths: Vec<(String, Clean)>,
    pub(super) truncated: bool,
}

pub(super) struct Found {
    pub(super) path: String,
    pub(super) matched: Vec<usize>,
    pub(super) display: Clean,
}

pub(super) fn index(root: &Path) -> Index {
    let paths = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .require_git(true)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".jj" | ".sl" | ".hg" | ".svn")
            )
        })
        .threads(4)
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                if count.load(Ordering::Relaxed) >= MAX_FILES {
                    return ignore::WalkState::Quit;
                }
                if let Ok(entry) = entry
                    && entry.file_type().is_some_and(|kind| kind.is_file())
                    && let Some(path) = entry.path().strip_prefix(root).ok().and_then(Path::to_str)
                {
                    count.fetch_add(1, Ordering::Relaxed);
                    paths
                        .lock()
                        .expect(crate::NOT_POISONED)
                        .push(path.to_owned());
                }
                ignore::WalkState::Continue
            })
        });
    let mut paths = paths.into_inner().expect(crate::NOT_POISONED);
    let truncated = paths.len() >= MAX_FILES;
    paths.truncate(MAX_FILES);
    paths.sort_unstable();
    Index {
        paths: paths
            .into_iter()
            .map(|path| {
                let display = Clean::new(&path);
                (path, display)
            })
            .collect(),
        truncated,
    }
}

impl Index {
    pub(super) fn matches(&self, query: &str) -> Vec<Found> {
        let matcher = SkimMatcherV2::default().ignore_case();
        let mut ranked: Vec<_> = self
            .paths
            .iter()
            .filter_map(|path| {
                matcher
                    .fuzzy_match(&path.1, query)
                    .map(|score| (score, path))
            })
            .collect();
        ranked.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.0.len().cmp(&b.1.0.len()))
                .then(a.1.0.cmp(&b.1.0))
        });
        ranked
            .into_iter()
            .take(SHOWN)
            .map(|(_, path)| Found {
                path: path.0.clone(),
                display: path.1.clone(),
                matched: matcher
                    .fuzzy_indices(&path.1, query)
                    .map(|(_, at)| at)
                    .unwrap_or_default(),
            })
            .collect()
    }
}
