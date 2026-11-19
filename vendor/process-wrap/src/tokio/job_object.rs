use std::{
	future::Future,
	io::{Error, ErrorKind, Result},
	ops::ControlFlow,
	os::windows::io::{AsRawHandle, BorrowedHandle},
	pin::Pin,
	process::ExitStatus,
	time::Duration,
};

use futures::channel::oneshot;
use tokio::{process::Command, task::spawn_blocking};
#[cfg(feature = "tracing")]
use tracing::{debug, instrument};
use windows::Win32::{
	Foundation::{CloseHandle, HANDLE},
	System::Threading::PROCESS_CREATION_FLAGS,
};

use crate::{
	ChildExitStatus,
	windows::{
		JobPort, job_creation_flags, make_job_object, resume_threads, terminate_job, wait_on_job,
	},
};

#[cfg(feature = "creation-flags")]
use super::CreationFlags;
#[cfg(feature = "kill-on-drop")]
use super::KillOnDrop;
use super::{ChildWrapper, CommandWrap, CommandWrapper};

type JobWaitReceiver = oneshot::Receiver<Result<ControlFlow<()>>>;

/// Wrapper which creates a job object context for a `Command`.
///
/// This wrapper is only available on Windows.
///
/// It creates a Windows Job Object and associates the [`Command`] to it. This behaves analogously
/// to process groups on Unix or even cgroups on Linux, with the ability to restrict resource use.
/// See [Job Objects](https://docs.microsoft.com/en-us/windows/win32/procthread/job-objects).
///
/// This wrapper provides a child wrapper: [`JobObjectChild`].
///
/// [`CreationFlags`] may be registered before or after `JobObject`; process-wrap preserves its flags
/// and distinguishes explicit suspension from the temporary suspension needed for assignment.
#[derive(Clone, Copy, Debug)]
pub struct JobObject;

fn user_creation_flags(core: &CommandWrap) -> PROCESS_CREATION_FLAGS {
	#[cfg(feature = "creation-flags")]
	{
		core.get_wrap::<CreationFlags>()
			.map_or(PROCESS_CREATION_FLAGS(0), |flags| flags.0)
	}
	#[cfg(not(feature = "creation-flags"))]
	{
		let _ = core;
		PROCESS_CREATION_FLAGS(0)
	}
}

fn terminate_child(child: &mut dyn ChildWrapper) {
	let _ = child.start_kill();
}

fn child_process_handle(child: &dyn ChildWrapper) -> Option<BorrowedHandle<'_>> {
	child.process_handle().or_else(|| {
		child
			.try_inner_child()
			.and_then(|child| child.process_handle())
	})
}

impl CommandWrapper for JobObject {
	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(self)))]
	fn pre_spawn(&mut self, command: &mut Command, core: &CommandWrap) -> Result<()> {
		let policy = job_creation_flags(user_creation_flags(core));
		command.creation_flags(policy.flags.0);
		Ok(())
	}

	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(self)))]
	fn wrap_child(
		&mut self,
		mut inner: Box<dyn ChildWrapper>,
		core: &CommandWrap,
	) -> Result<Box<dyn ChildWrapper>> {
		#[cfg(feature = "kill-on-drop")]
		let kill_on_drop = core.has_wrap::<KillOnDrop>();
		#[cfg(not(feature = "kill-on-drop"))]
		let kill_on_drop = false;

		let policy = job_creation_flags(user_creation_flags(core));

		#[cfg(feature = "tracing")]
		debug!(
			?kill_on_drop,
			resume_after_assignment = policy.resume_after_assignment,
			"options from other wrappers"
		);

		// Prefer the explicit capability, while preserving composition with transparent wrappers
		// written before `process_handle` was added.
		let handle = match child_process_handle(inner.as_ref()) {
			Some(handle) => HANDLE(handle.as_raw_handle()),
			None => {
				terminate_child(&mut *inner);
				return Err(Error::new(
					ErrorKind::Unsupported,
					"child wrapper does not expose a Windows process handle",
				));
			}
		};

		let job_port = match make_job_object(handle, kill_on_drop) {
			Ok(job_port) => job_port,
			Err(error) => {
				terminate_child(&mut *inner);
				return Err(error);
			}
		};

		if policy.resume_after_assignment {
			if let Err(error) = resume_threads(handle) {
				let _ = terminate_job(job_port.job, 1);
				terminate_child(&mut *inner);
				return Err(error);
			}
		}

		Ok(Box::new(JobObjectChild::new(inner, job_port)))
	}
}

/// Wrapper for `Child` which waits on all processes within the job.
#[derive(Debug)]
pub struct JobObjectChild {
	inner: Box<dyn ChildWrapper>,
	exit_status: ChildExitStatus,
	job_port: JobPort,
	job_wait: Option<JobWaitReceiver>,
}

impl JobObjectChild {
	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(job_port)))]
	pub(crate) fn new(inner: Box<dyn ChildWrapper>, job_port: JobPort) -> Self {
		Self {
			inner,
			exit_status: ChildExitStatus::Running,
			job_port,
			job_wait: None,
		}
	}
}

impl ChildWrapper for JobObjectChild {
	fn inner(&self) -> &dyn ChildWrapper {
		self.inner.as_ref()
	}
	fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
		self.inner.as_mut()
	}
	fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
		// manually drop the completion port
		let its = std::mem::ManuallyDrop::new(self.job_port);
		unsafe { CloseHandle(its.completion_port.0) }.ok();
		// we leave the job handle unclosed, otherwise the Child is useless
		// (as closing it will terminate the job)

		self.inner
	}
	fn process_handle(&self) -> Option<BorrowedHandle<'_>> {
		child_process_handle(self.inner.as_ref())
	}

	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(self)))]
	fn start_kill(&mut self) -> Result<()> {
		terminate_job(self.job_port.job, 1)
	}

	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(self)))]
	fn wait(&mut self) -> Pin<Box<dyn Future<Output = Result<ExitStatus>> + Send + '_>> {
		Box::pin(async {
			if let ChildExitStatus::Exited(status) = &self.exit_status {
				return Ok(*status);
			}

			const MAX_RETRY_ATTEMPT: usize = 10;

			// always wait for parent to exit first, as by the time it does,
			// it's likely that all its children have already exited.
			let status = self.inner.wait().await?;

			if self.job_wait.is_none() {
				// nevertheless, now try reaping all children a few times...
				for _ in 1..MAX_RETRY_ATTEMPT {
					if wait_on_job(self.job_port.completion_port, Some(Duration::ZERO))?.is_break()
					{
						self.exit_status = ChildExitStatus::Exited(status);
						return Ok(status);
					}
				}

				// ...finally, if there are some that are still alive,
				// block in the background to reap them fully. Keep the receiver on
				// the child so cancellation cannot lose a completion the blocking
				// waiter consumes.
				let completion_port = self.job_port.completion_port;
				let (send, receive) = oneshot::channel();
				drop(spawn_blocking(move || {
					let _ = send.send(wait_on_job(completion_port, None));
				}));
				self.job_wait = Some(receive);
			}
			let result = self
				.job_wait
				.as_mut()
				.expect("Job completion receiver is present")
				.await;
			self.job_wait = None;
			match result.map_err(Error::other)?? {
				ControlFlow::Break(()) => {}
				ControlFlow::Continue(()) => {
					return Err(Error::other("infinite Job wait returned a timeout"));
				}
			}
			self.exit_status = ChildExitStatus::Exited(status);
			Ok(status)
		})
	}

	#[cfg_attr(feature = "tracing", instrument(level = "debug", skip(self)))]
	fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		if let ChildExitStatus::Exited(status) = &self.exit_status {
			return Ok(Some(*status));
		}

		let Some(status) = self.inner.try_wait()? else {
			return Ok(None);
		};
		let completion = if self.job_wait.is_some() {
			let received = self
				.job_wait
				.as_mut()
				.expect("Job completion receiver is present")
				.try_recv();
			match received {
				Ok(Some(result)) => {
					self.job_wait = None;
					result?
				}
				Ok(None) => return Ok(None),
				Err(error) => {
					self.job_wait = None;
					return Err(Error::other(error));
				}
			}
		} else {
			wait_on_job(self.job_port.completion_port, Some(Duration::ZERO))?
		};
		if completion.is_continue() {
			return Ok(None);
		}

		self.exit_status = ChildExitStatus::Exited(status);
		Ok(Some(status))
	}
}
