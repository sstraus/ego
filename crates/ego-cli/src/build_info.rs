//! One packaged source for what this build is.
//!
//! `ego --version`, `/version` and the ACP `initialize` answer all describe the
//! same binary, and before this module each of them spelled it out separately.
//! That is three chances to disagree about one fact, and the interesting half
//! of the answer — which protocol and which schema a host will actually meet —
//! was in none of them.
//!
//! Everything here is a compile-time constant. There is no build script and no
//! git hash: a version that changes when the working tree is dirty tells a bug
//! reporter something ego cannot then look up, and the package version plus the
//! wire versions is what anyone acting on the report actually needs.

/// The package version, which is what a release is named after.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the notes for this release live.
///
/// A URL and not a bundled file: notes are written after the binary is cut, so
/// a copy compiled into it would be the one thing guaranteed to be out of date.
pub const RELEASE_NOTES: &str = "https://github.com/sstraus/ego/releases";

/// The single sentence every surface prints.
///
/// The ACP version is read from `ego-acp`'s own constant rather than restated,
/// so a build that starts serving something else says so without anybody
/// remembering to edit a string here.
pub fn summary() -> String {
    format!(
        "ego {VERSION} — ACP protocol {} — release notes: {RELEASE_NOTES}",
        ego_acp::negotiate::SERVED
    )
}
