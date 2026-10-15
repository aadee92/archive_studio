fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile ui/app.slint");

    // The Slint version in use, for the log's startup lines.
    let lock = std::fs::read_to_string("../Cargo.lock").unwrap_or_default();
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"slint\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
        .map_or("unknown", |v| v.trim_end_matches('"'));
    println!("cargo:rustc-env=SLINT_VERSION={version}");
    println!("cargo:rerun-if-changed=../Cargo.lock");
}
