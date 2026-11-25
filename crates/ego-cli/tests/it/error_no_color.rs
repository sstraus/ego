// Catches: --no-color is parsed but the stderr error adapter still emits red SGR.
#[test]
fn no_color_flag_keeps_real_tty_error_plain() {
    use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
    use std::io::Read;

    let scratch = tempfile::tempdir().unwrap();
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    command.args(["--no-color", "--profile", "ci"]);
    command.cwd(scratch.path());
    command.env("EGO_HOME", scratch.path().join("home"));
    command.env("TERM", "xterm-256color");
    command.env_remove("NO_COLOR");
    command.env_remove("EGO_LOG");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let reading = std::thread::spawn(move || {
        let mut output = String::new();
        let _ = reader.read_to_string(&mut output);
        output
    });
    let status = child.wait().unwrap();
    // A Windows pseudoconsole ends its output only when it is closed.
    drop(pair.master);
    let output = reading.join().unwrap();
    assert!(!status.success(), "{output:?}");
    assert!(
        output.contains("profile 'ci' is available only through"),
        "{output:?}"
    );
    assert!(!output.contains("\x1b[31m"), "{output:?}");
}
