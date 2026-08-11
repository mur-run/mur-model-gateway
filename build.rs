fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_beta_hook)");
    println!("cargo::rustc-check-cfg=cfg(has_codex_hook)");
    // Re-run when a gitignored impl appears/disappears, else Cargo's cached
    // "file absent" result sticks and a freshly-restored impl stays stubbed.
    println!("cargo::rerun-if-changed=src/disguise/disguise_impl.rs");
    println!("cargo::rerun-if-changed=src/codex/codex_impl.rs");
    if std::path::Path::new("src/disguise/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
    if std::path::Path::new("src/codex/codex_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_codex_hook");
    }
}
