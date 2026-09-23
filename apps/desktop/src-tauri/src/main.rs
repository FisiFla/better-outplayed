// Release builds of a desktop app must not open a console window on Windows, but a debug
// build should keep it: that console is where `tracing` and the startup errors land.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    localplay_desktop::run()
}
