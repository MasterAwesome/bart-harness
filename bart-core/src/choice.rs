use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

const MAX_BYTES: u64 = 4096;

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("last-choice.json")
}

pub fn load(state_dir: &Path) -> io::Result<Option<Choice>> {
    let path = path(state_dir);
    crate::persist::read(&path, MAX_BYTES)?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
        })
        .transpose()
}

pub fn save(state_dir: &Path, choice: &Choice) -> Result<(), crate::persist::ReplaceError> {
    crate::persist::replace_json(&path(state_dir), choice, MAX_BYTES)
}
