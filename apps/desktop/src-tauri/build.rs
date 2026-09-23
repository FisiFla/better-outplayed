fn main() {
    // `tauri_build::build()` is what expands `tauri.conf.json` into the compile-time
    // context `tauri::generate_context!` reads (window config, the asset-protocol scope,
    // the capability set) and, on Windows, embeds the manifest and icon into the binary.
    tauri_build::build()
}
