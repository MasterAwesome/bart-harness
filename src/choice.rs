use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Choice {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

const MAX_BYTES: u64 = 4096;

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("last-choice.json")
}

pub(crate) fn load(state_dir: &Path) -> io::Result<Option<Choice>> {
    let path = path(state_dir);
    crate::persist::read(&path, MAX_BYTES)?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
        })
        .transpose()
}

pub(crate) fn save(state_dir: &Path, choice: &Choice) -> Result<(), crate::persist::ReplaceError> {
    crate::persist::replace_json(&path(state_dir), choice, MAX_BYTES)
}
