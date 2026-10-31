// These tests start Bart as a child process. Cargo supplies its executable path
// to integration tests, but not to unit tests.
include!("../src/main.rs");
