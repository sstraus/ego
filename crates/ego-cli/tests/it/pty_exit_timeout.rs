use crate::support;

#[cfg(unix)]
#[test]
fn a_pty_child_that_ignores_eof_fails_with_a_bounded_transcript() {
    use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("fixture PTY");
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", "printf fixture-terminal-marker; exec /bin/sleep 30"]);
    let child = pair
        .slave
        .spawn_command(command)
        .expect("non-exiting fixture");
    let mut killer = child.clone_killer();
    drop(pair.slave);
    let mut transcript = [0; b"fixture-terminal-marker".len()];
    pair.master
        .try_clone_reader()
        .expect("fixture PTY reader")
        .read_exact(&mut transcript)
        .expect("fixture terminal output");

    let (sent, received) = mpsc::channel();
    let start = Instant::now();
    let waiter = std::thread::spawn(move || {
        sent.send(support::wait_pty_child(
            child,
            Duration::from_millis(100),
            &transcript,
        ))
        .expect("wait result receiver");
    });
    let result = match received.recv_timeout(Duration::from_secs(3)) {
        Ok(result) => result,
        Err(error) => {
            killer.kill().expect("stop the hanging fixture");
            waiter.join().expect("waiter after fixture cleanup");
            panic!("PTY child wait exceeded its deadline: {error}");
        }
    };
    waiter.join().expect("waiter");
    let diagnostic = result.expect_err("the sleeping child must fail its exit deadline");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(
        diagnostic.contains("fixture-terminal-marker"),
        "{diagnostic}"
    );
}
