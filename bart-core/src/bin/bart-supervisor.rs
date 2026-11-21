use std::{env, process::ExitCode};

fn main() -> ExitCode {
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == bart_core::supervise::DETACHED_MODE)
    {
        return bart_core::supervise::detached_main();
    }
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == bart_core::supervise::MODE)
    {
        return bart_core::supervise::main();
    }
    ExitCode::FAILURE
}
