#![allow(unreachable_pub)]
// These tests start the supervisor as a child process. Cargo supplies its
// executable path to integration tests, but not to unit tests.
include!("../src/lib.rs");
