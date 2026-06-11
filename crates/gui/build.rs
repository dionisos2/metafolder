fn main() {
    // include_str!/include_dir! do not register their inputs with cargo:
    // re-embed the shim, panels and default config when they change.
    println!("cargo:rerun-if-changed=panel-shim");
    println!("cargo:rerun-if-changed=panel-types");
    println!("cargo:rerun-if-changed=default-config");
    println!("cargo:rerun-if-changed=frontend/dist");

    // The tauri context macro requires `frontendDist` to exist at compile
    // time; a placeholder keeps plain `cargo build` working before the
    // frontend has been built (`npm --prefix frontend run build`).
    let dist = std::path::Path::new("frontend/dist");
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(dist).expect("create frontend/dist");
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><!-- placeholder: run `npm run build` in frontend/ -->\n",
        )
        .expect("write placeholder index.html");
    }
    tauri_build::build();
}
