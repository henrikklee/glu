use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_DEV_REGISTRY");

    if env::var_os("CARGO_FEATURE_DEV_REGISTRY").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let workspace_dir = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("glu-cli must remain under the workspace crates directory");
    let production_release_dir = workspace_dir.join("target").join("release");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));

    if out_dir.starts_with(&production_release_dir) {
        eprintln!("error: dev-registry cannot be built into target/release; use `cargo build-dev`");
        std::process::exit(1);
    }
}
