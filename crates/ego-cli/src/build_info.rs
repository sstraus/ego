//! One packaged source for what this build is.
//!
//! `ego --version`, `/version` and the ACP `initialize` answer all describe the
//! same binary, and before this module each of them spelled it out separately.
//! That is three chances to disagree about one fact, and the interesting half
//! of the answer — which protocol and which schema a host will actually meet —
//! was in none of them.
//!
//! Everything here is a compile-time constant. The build script adds the commit
//! the binary was cut from and the profile, so an owner who runs many builds of
//! one version can tell which of them hosts a session. It records the commit and
//! never a dirty flag: a version that changes with the working tree tells a bug
//! reporter something ego cannot then look up.

/// The package version, which is what a release is named after.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The short commit this binary was built from, or `unknown` outside a checkout.
pub const COMMIT: &str = env!("EGO_BUILD_COMMIT");

/// `debug` or `release`.
pub const PROFILE: &str = env!("EGO_BUILD_PROFILE");

/// The startup line: version, commit and profile of the running binary.
pub fn identity() -> String {
    format!("ego {VERSION} \u{b7} {COMMIT} \u{b7} {PROFILE}")
}

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
        "ego {VERSION} ({COMMIT}, {PROFILE}) — ACP protocol {} — release notes: {RELEASE_NOTES}",
        ego_acp::negotiate::SERVED
    )
}
