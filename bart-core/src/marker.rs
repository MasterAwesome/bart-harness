pub const PREFIX: &str = "[Bart ";

pub fn open(kind: &str, fields: &str) -> String {
    format!("{PREFIX}{kind}: {}", fields.replace(['\n', '\r'], " "))
}

pub fn line(kind: &str, fields: &str) -> String {
    let mut line = open(kind, fields);
    line.push(']');
    line
}

#[macro_export]
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
    (const Cell, $text:literal) => { concat!("[Bart cell: ", $text, "]") };
    (const History, $text:literal) => { concat!("[Bart history: ", $text, "]") };
    (const Image, $text:literal) => { concat!("[Bart image: ", $text, "]") };
    (const Job, $text:literal) => { concat!("[Bart job: ", $text, "]") };
    (const Mcp, $text:literal) => { concat!("[Bart mcp: ", $text, "]") };
    (const NotRun, $text:literal) => { concat!("[Bart not run: ", $text, "]") };
    (const Output, $text:literal) => { concat!("[Bart output: ", $text, "]") };
    (const Project, $text:literal) => { concat!("[Bart project: ", $text, "]") };
    (const Run, $text:literal) => { concat!("[Bart run: ", $text, "]") };
    (const Session, $text:literal) => { concat!("[Bart session: ", $text, "]") };
    (const Skill, $text:literal) => { concat!("[Bart skill: ", $text, "]") };
    (const Summary, $text:literal) => { concat!("[Bart summary: ", $text, "]") };
    (const UserMessage, $text:literal) => { concat!("[Bart user message: ", $text, "]") };
    (const WebSearch, $text:literal) => { concat!("[Bart web_search: ", $text, "]") };
    (Cell, $($arg:tt)*) => { $crate::marker::line("cell", &format!($($arg)*)) };
    (History, $($arg:tt)*) => { $crate::marker::line("history", &format!($($arg)*)) };
    (Image, $($arg:tt)*) => { $crate::marker::line("image", &format!($($arg)*)) };
    (Job, $($arg:tt)*) => { $crate::marker::line("job", &format!($($arg)*)) };
    (Mcp, $($arg:tt)*) => { $crate::marker::line("mcp", &format!($($arg)*)) };
    (NotRun, $($arg:tt)*) => { $crate::marker::line("not run", &format!($($arg)*)) };
    (Output, $($arg:tt)*) => { $crate::marker::line("output", &format!($($arg)*)) };
    (Project, $($arg:tt)*) => { $crate::marker::line("project", &format!($($arg)*)) };
    (Run, $($arg:tt)*) => { $crate::marker::line("run", &format!($($arg)*)) };
    (Session, $($arg:tt)*) => { $crate::marker::line("session", &format!($($arg)*)) };
    (Skill, $($arg:tt)*) => { $crate::marker::line("skill", &format!($($arg)*)) };
    (Summary, $($arg:tt)*) => { $crate::marker::line("summary", &format!($($arg)*)) };
    (UserMessage, $($arg:tt)*) => { $crate::marker::line("user message", &format!($($arg)*)) };
    (WebSearch, $($arg:tt)*) => { $crate::marker::line("web_search", &format!($($arg)*)) };
}
