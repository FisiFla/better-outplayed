// Release builds of a desktop app must not open a console window on Windows, but a debug
// build should keep it: that console is where `tracing` and the startup errors land.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

fn main() {
    localplay_desktop::run()
}
