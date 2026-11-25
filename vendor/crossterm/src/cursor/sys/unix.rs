use std::{
    io::{self, Error, ErrorKind, Write},
    time::{Duration, Instant},
};

use crate::{
    event::{
        filter::CursorPositionFilter, lock_internal_event_reader, poll_internal, read_internal,
        InternalEvent,
    },
    terminal::{disable_raw_mode, enable_raw_mode, sys::is_raw_mode_enabled},
};

/// Returns the cursor position (column, row).
///
/// The top left cell is represented as `(0, 0)`.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
pub fn position() -> io::Result<(u16, u16)> {
    if is_raw_mode_enabled() {
        read_position_raw()
    } else {
        read_position()
    }
}

fn read_position() -> io::Result<(u16, u16)> {
    enable_raw_mode()?;
    let pos = read_position_raw();
    disable_raw_mode()?;
    pos
}

fn read_position_raw() -> io::Result<(u16, u16)> {
    // Replies to timed-out queries can remain queued behind pasted input.
    // They describe an earlier screen, not the position requested below.
    // Retain input events, but discard reports already available before DSR.
    lock_internal_event_reader().discard_pending(&CursorPositionFilter)?;

    // Use `ESC [ 6 n` to and retrieve the cursor position.
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1B[6n")?;
    stdout.flush()?;

    // The event source can wake without a cursor report (for example when a
    // resize or a host repaint interrupts its poll). That is not the query's
    // deadline. Keep one bounded budget across those unrelated wakes.
    let deadline = Instant::now() + Duration::from_millis(2000);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::new(
                ErrorKind::TimedOut,
                "The cursor position could not be read within a normal duration",
            ));
        }
        match poll_internal(Some(remaining), &CursorPositionFilter) {
            Ok(true) => {
                if let Ok(InternalEvent::CursorPosition(x, y)) =
                    read_internal(&CursorPositionFilter)
                {
                    return Ok((x, y));
                }
            }
            Ok(false) => {}
            Err(error) => return Err(error),
        }
    }
}
