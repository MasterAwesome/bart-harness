use std::path::{Component, Path};

use crate::history::Skill;

const MAX_BYTES: u64 = 64 * 1024;

pub(super) fn resolve(text: &str) -> Result<Option<Skill>, String> {
    let Some(command) = text.strip_prefix('/') else {
        return Ok(None);
    };
    let name = command.split(char::is_whitespace).next().unwrap_or("");
    let home = std::env::home_dir().ok_or("cannot locate the home directory for skills")?;
    load(&home.join(".agents/skills"), name)
}

fn load(root: &Path, name: &str) -> Result<Option<Skill>, String> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Ok(None);
    }
    let path = root.join(name).join("SKILL.md");
    let Some(bytes) = crate::persist::read(&path, MAX_BYTES)
        .map_err(|error| format!("cannot load skill /{name} at {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    let body = String::from_utf8(bytes).map_err(|_| format!("skill /{name} must be UTF-8 text"))?;
    if body.trim().is_empty() {
        return Err(format!("skill /{name} is empty"));
    }
    Ok(Some(Skill {
        name: name.to_owned(),
        text: body.into(),
        path,
    }))
}

#[cfg(test)]
mod tests;
