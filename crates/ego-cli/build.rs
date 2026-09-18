fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => println!("cargo::rustc-link-arg-bin=ego=/STACK:8388608"),
        Ok("gnu") => println!("cargo::rustc-link-arg-bin=ego=-Wl,--stack,8388608"),
        _ => {}
    }
}
