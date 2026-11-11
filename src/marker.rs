pub(crate) const PREFIX: &str = "[Bart ";

pub(crate) fn open(kind: &str, fields: &str) -> String {
    format!("{PREFIX}{kind}: {}", fields.replace(['\n', '\r'], " "))
}

pub(crate) fn line(kind: &str, fields: &str) -> String {
    let mut line = open(kind, fields);
    line.push(']');
    line
}

macro_rules! bart {
    (@name Cell) => { "cell" };
    (@name History) => { "history" };
    (@name Image) => { "image" };
    (@name Job) => { "job" };
    (@name Mcp) => { "mcp" };
    (@name NotRun) => { "not run" };
    (@name Output) => { "output" };
    (@name Project) => { "project" };
    (@name Run) => { "run" };
    (@name Session) => { "session" };
    (@name Skill) => { "skill" };
    (@name Summary) => { "summary" };
    (@name UserMessage) => { "user message" };
    (@name WebSearch) => { "web_search" };
    (const $kind:ident, $text:literal) => {
        concat!("[Bart ", bart!(@name $kind), ": ", $text, "]")
    };
    ($kind:ident, $($arg:tt)*) => {
        $crate::marker::line(bart!(@name $kind), &format!($($arg)*))
    };
}
