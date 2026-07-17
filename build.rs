fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_beta_hook)");
    // Re-run when the gitignored impl appears/disappears, else Cargo's cached
    // "file absent" result sticks and a freshly-restored impl stays stubbed.
    println!("cargo::rerun-if-changed=src/disguise/disguise_impl.rs");
    if std::path::Path::new("src/disguise/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
}
