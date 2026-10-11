//! Require a real, successful libtest child run rather than an exit code alone.

pub fn assert_one_child_test(output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "child must pass exactly one test\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
