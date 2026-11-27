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
    let mut writer = pair.master.take_writer().unwrap();
    let reading = std::thread::spawn(move || {
        // A Windows pseudoconsole holds its child until the host answers the
        // startup cursor query (row 2: ConPTY reads a row-1 reply as a key).
        use std::io::Write;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut answered = 0;
        while let Ok(length) = reader.read(&mut chunk) {
            if length == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..length]);
            let requests = bytes.windows(4).filter(|w| w == b"\x1b[6n").count();
            for _ in answered..requests {
                if writer
                    .write_all(b"\x1b[2;1R")
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            answered = requests;
        }
        String::from_utf8_lossy(&bytes).into_owned()
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
