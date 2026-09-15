//! `process`, and the facts `require('node:os')` reports.
//!
//! The line this draws is the one between a script that knows where it is and a
//! script that can act there. The host's name, its platform, its temporary
//! directory and its environment are facts a notification may legitimately echo
//! -- a message that says which machine it came from is the whole point of the
//! channel -- while a file descriptor, a child process or a socket is a
//! capability it has no business having. `require` therefore answers for a few
//! modules that only compute and format, and refuses everything else by name.
//!
//! There is no `process.exit`. The reference implementation has one, and it
//! unwinds the whole runtime; here the engine is one event's, so a script that
//! wants to stop ends by returning or by throwing, and there is nothing a
//! `process.exit` could usefully mean.

use std::collections::BTreeMap;

use rquickjs::Ctx;
use serde::Serialize;

use super::native;

/// What the bootstrap builds `process` and `os` from.
///
/// One JSON string rather than a handful of objects assembled in Rust: the
/// bootstrap already speaks JSON, and a value handed over as text cannot carry a
/// live host object along with it.
#[derive(Serialize)]
struct Info {
    /// The hub's environment, as a snapshot.
    env: BTreeMap<String, String>,
    platform: &'static str,
    arch: &'static str,
    /// `None` when the host refuses to say, which `os.hostname()` then reports
    /// as an empty string rather than as a failure.
    hostname: Option<String>,
    tmpdir: String,
    eol: &'static str,
    version: String,
    versions: BTreeMap<String, String>,
    argv: Vec<String>,
}

/// Installs the process facts under the name the bootstrap picks up.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    native(ctx, "__processInfo", process_info)
}

/// Everything a script may know about the process it is running in.
fn process_info() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let info = Info {
        env: std::env::vars().collect(),
        platform: platform(),
        arch: arch(),
        hostname: hostname(),
        tmpdir: std::env::temp_dir().to_string_lossy().into_owned(),
        eol: if cfg!(windows) { "\r\n" } else { "\n" },
        // Spelled the way Node spells its own, because that is what a script
        // comparing versions will have been written against.
        version: format!("v{version}"),
        versions: BTreeMap::from([("monitor".to_owned(), version.to_owned())]),
        // Not the hub's own command line. It is the hub's, not the script's, and
        // it can carry paths and flags that an operator pasting a script into a
        // panel never meant to hand over; two entries that name the host program
        // and the script are what a script can meaningfully read.
        argv: vec!["monitor-hub".to_owned(), "notify.js".to_owned()],
    };
    // A struct of strings cannot fail to serialize; the fallback exists so that
    // the impossible case is an empty process rather than a panic.
    serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_owned())
}

/// The host's name, the way `os.hostname()` reports it.
///
/// Through `libc` rather than an environment variable or a file: the release
/// image is `FROM scratch`, so there is no `/etc/hostname` to read and no
/// `hostname(1)` to run, while this is one syscall that needs neither.
fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: `gethostname` writes at most the length it is given into the
    // buffer and reports failure by returning -1, in which case nothing is read
    // back. The buffer is a local that outlives the call.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    // POSIX leaves the name unterminated when it is too long for the buffer, so
    // a missing terminator means "the whole buffer", not "empty".
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// The platform, spelled the way Node spells it.
///
/// Node's `process.platform` is `darwin` where Rust says `macos`, and a script
/// that branches on the value is branching on Node's spelling.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// The architecture, spelled the way Node spells it.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}
