pub fn root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".ego-admission-")
        .tempdir()
        .unwrap()
}
