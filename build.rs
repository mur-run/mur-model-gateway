fn main() {
    if std::path::Path::new("src/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
}
