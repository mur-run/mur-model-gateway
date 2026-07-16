fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_beta_hook)");
    if std::path::Path::new("src/disguise/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
}
