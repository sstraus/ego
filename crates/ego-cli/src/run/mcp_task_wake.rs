//! Wake an idle interactive session when a live MCP task is due for a poll.
//!
//! The timer only wakes the seat; it sends nothing. The seat's dispatcher then
//! polls the due tasks through [`ego_core::mcp::McpTaskPort::poll_due`] with the
//! session's guarded effect ports, and starts one generation only when a task
//! settled. A wake that finds the task still working starts none.

use std::sync::Arc;
use std::time::Duration;

use ego_core::mcp::McpTaskPort;

/// The shortest time between two wakes. A busy seat does not read a wake
/// until its turn ends, so without this floor a past due time would wake it
/// in a loop. The ledger never polls a task faster than this either.
const FLOOR: Duration = Duration::from_millis(500);

/// How often an empty ledger is checked for a new task.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// The running timer; dropping it stops the timer.
pub(super) struct McpTaskWake(tokio::task::JoinHandle<()>);

impl Drop for McpTaskWake {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl McpTaskWake {
    pub(super) fn start(port: Arc<dyn McpTaskPort>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self(tokio::spawn(async move {
            loop {
                let wait = match port.next_due() {
                    Some(due) => due.saturating_duration_since(std::time::Instant::now()),
                    None => IDLE_CHECK,
                };
                tokio::time::sleep(wait.max(FLOOR)).await;
                if port
                    .next_due()
                    .is_some_and(|due| due <= std::time::Instant::now())
                {
                    wake();
                }
            }
        }))
    }
}
