fn main() {
    let module = "assets/js-engine/plugin.wasm";
    println!("cargo::rerun-if-changed={module}");
    if !std::path::Path::new(module).exists() {
        println!("cargo::error={module} is missing: fetch it with `just desktop-js-engine`");
    }
}
