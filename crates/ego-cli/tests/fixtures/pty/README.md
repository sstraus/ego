# PTY EOF regression capture

`typed-fork-before-eof.ansi` records a Linux ARM64 terminal repaint before EOF.
The last complete frame still contains `/fork`. Counting its `> ` as a new
ready prompt sends EOF before the input driver is ready. This fixture protects
against treating a typed-line repaint or an incomplete redraw as readiness.

The bytes are recorded terminal output, not an invented screen. Machine and
local artifact identifiers are omitted from this description.
