fn main() {
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
