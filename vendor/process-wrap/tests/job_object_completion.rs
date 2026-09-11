#![cfg(all(
	windows,
	feature = "tokio1",
	feature = "job-object",
	feature = "kill-on-drop"
))]

//! Contract for the corrected process-wrap Job completion barrier.
//!
//! The fixture communicates over the inherited stdout pipe. This keeps the
//! test on process-wrap's public API: no PID polling, taskkill, private Job
//! handles, or test-only spawn seams are involved.

use std::io::{Result, Write};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::time::timeout;

const DESCENDANT_LIFETIME: Duration = Duration::from_millis(500);
const SHORT_WAIT: Duration = Duration::from_millis(100);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

fn fixture_command(mode: &str) -> CommandWrap {
	let executable = std::env::current_exe().expect("test executable");
	let mut command = CommandWrap::with_new(executable, |command| {
		command
			.args([
				"--ignored",
				"--exact",
				"job_object_completion_fixture",
				"--nocapture",
				"--",
				mode,
			])
			.stdout(Stdio::piped())
			.stderr(Stdio::null());
	});
	command.wrap(KillOnDrop).wrap(JobObject);
	command
}

fn spawn_fixture(mode: &str) {
	StdCommand::new(std::env::current_exe().expect("test executable"))
		.args([
			"--ignored",
			"--exact",
			"job_object_completion_fixture",
			"--nocapture",
			"--",
			mode,
		])
		.stdin(Stdio::null())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.spawn()
		.expect("descendant fixture");
}

#[test]
#[ignore = "child fixture"]
fn job_object_completion_fixture() {
	let mut args = std::env::args_os().skip_while(|arg| arg != "--").skip(1);
	let mode = args.next().expect("fixture mode");

	match mode.to_string_lossy().as_ref() {
		"root-exits-before-descendant" => {
			spawn_fixture("descendant-short");
			println!("ROOT_EXITED");
			std::io::stdout().flush().expect("root marker flush");
		}
		"descendant-short" => {
			println!("DESCENDANT_STARTED");
			std::io::stdout().flush().expect("descendant start flush");
			std::thread::sleep(DESCENDANT_LIFETIME);
			println!("DESCENDANT_DONE");
			std::io::stdout()
				.flush()
				.expect("descendant completion flush");
		}
		"root-stubborn" => {
			spawn_fixture("descendant-stubborn");
			println!("ROOT_READY");
			std::io::stdout().flush().expect("ready marker flush");
			std::thread::park();
		}
		"descendant-stubborn" => std::thread::park(),
		other => panic!("unknown fixture mode {other:?}"),
	}
}

async fn next_line(reader: &mut BufReader<tokio::process::ChildStdout>, limit: Duration) -> String {
	let mut line = String::new();
	let bytes = timeout(limit, reader.read_line(&mut line))
		.await
		.expect("fixture output deadline")
		.expect("fixture output");
	assert_ne!(bytes, 0, "fixture stdout closed before its marker");
	line
}

#[tokio::test]
async fn wait_and_try_wait_require_active_process_zero_and_then_stay_stable() -> Result<()> {
	let mut command = fixture_command("root-exits-before-descendant");
	let mut child = command.spawn()?;
	let stdout = child.stdout().take().expect("fixture stdout");
	let mut stdout = BufReader::new(stdout);

	assert_eq!(
		next_line(&mut stdout, COMPLETION_TIMEOUT).await,
		"ROOT_EXITED\n"
	);
	assert_eq!(
		next_line(&mut stdout, COMPLETION_TIMEOUT).await,
		"DESCENDANT_STARTED\n"
	);

	for _ in 0..3 {
		assert!(
			child.try_wait()?.is_none(),
			"root exit is not Job completion"
		);
	}
	assert!(
		timeout(SHORT_WAIT, child.wait()).await.is_err(),
		"wait completed before ACTIVE_PROCESS_ZERO"
	);

	assert_eq!(
		next_line(&mut stdout, COMPLETION_TIMEOUT).await,
		"DESCENDANT_DONE\n"
	);
	let status = timeout(COMPLETION_TIMEOUT, child.wait())
		.await
		.expect("Job completion deadline")?;
	assert!(status.success(), "root status should remain successful");

	let repeated = timeout(SHORT_WAIT, child.wait())
		.await
		.expect("completed wait blocked")?;
	assert_eq!(repeated.code(), status.code());
	assert_eq!(repeated.success(), status.success());
	for _ in 0..3 {
		assert_eq!(
			child
				.try_wait()?
				.expect("completed try_wait pending")
				.code(),
			status.code()
		);
	}

	let mut trailing = Vec::new();
	timeout(COMPLETION_TIMEOUT, stdout.read_to_end(&mut trailing))
		.await
		.expect("descendant stdout did not close")?;
	assert!(
		trailing.is_empty(),
		"unexpected fixture output after completion"
	);
	Ok(())
}

#[tokio::test]
async fn bounded_wait_then_kill_closes_the_owned_job_and_pipe() -> Result<()> {
	let mut command = fixture_command("root-stubborn");
	let mut child = command.spawn()?;
	let stdout = child.stdout().take().expect("fixture stdout");
	let mut stdout = BufReader::new(stdout);

	assert_eq!(
		next_line(&mut stdout, COMPLETION_TIMEOUT).await,
		"ROOT_READY\n"
	);
	assert!(
		timeout(SHORT_WAIT, child.wait()).await.is_err(),
		"bounded wait ignored its deadline"
	);

	child.start_kill()?;
	let _ = timeout(COMPLETION_TIMEOUT, child.wait())
		.await
		.expect("killed Job completion deadline")?;

	let mut trailing = Vec::new();
	timeout(COMPLETION_TIMEOUT, stdout.read_to_end(&mut trailing))
		.await
		.expect("owned descendant kept stdout open")?;
	assert!(trailing.is_empty(), "unexpected fixture output after kill");
	Ok(())
}
