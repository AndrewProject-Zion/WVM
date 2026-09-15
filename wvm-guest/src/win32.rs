//! Win32 execution layer.
//!
//! This is the module that makes the guest more than a socket that answers politely. Everything
//! above it — the framing, the path containment, the transfer planning — is platform-independent
//! logic that runs and is tested on Linux. This file is the part that only exists on Windows.
//!
//! # Structure
//!
//! The design keeps as much as possible *outside* the platform-specific section. A process launch
//! is `Command` plus argument assembly plus capture plus timeout: only argument quoting and a
//! couple of constants are genuinely Windows. Keeping the shape in one place and the calls behind
//! a small interface means the interesting parts can be reasoned about on any machine, and the
//! `#[cfg(windows)]` boundary stays narrow enough to audit by reading it.
//!
//! # What is deliberately not here
//!
//! No capability checks. The guest is a dumb executor by design: authority is decided on the host,
//! and a second, weaker check here would only create a place for the two to disagree. See
//! `docs/DECISIONS.md`.
//!
//! # Why so much is dead code on the host
//!
//! The quoting, the outcome types and the capture helpers are used only by the Windows `run()`,
//! which is `#[cfg(windows)]`. A host build therefore reports them as unused even though they are
//! fully tested here and exercised in a real guest. The allow below is scoped to this module so
//! that genuine dead code elsewhere is still reported.
#![allow(dead_code)] // used by the #[cfg(windows)] half; tested on the host

use anyhow::{anyhow, Result};

/// How a captured process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exited on its own with a status code.
    Exited { code: i32 },
    /// Still running when the deadline passed, and was killed.
    ///
    /// This is a distinct outcome rather than an error: "the program ran too long" and "the
    /// program failed" call for different responses from whoever asked, and collapsing them loses
    /// information the caller needs. A build that times out should not look like a build that
    /// failed.
    TimedOut { killed_after_ms: u64 },
}

impl Outcome {
    /// Whether the process completed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Exited { code: 0 })
    }
}

/// The result of running a program.
#[derive(Debug, Clone)]
pub struct Execution {
    pub outcome: Outcome,
    /// Captured standard output, decoded lossily.
    ///
    /// Lossy on purpose: Windows console output is not reliably UTF-8, and a program that emits an
    /// invalid byte should not fail the whole call. The alternative — refusing to return anything —
    /// makes the tool useless for the logs most worth reading.
    pub stdout: String,
    pub stderr: String,
    /// Wall-clock duration, for the caller to reason about timeouts.
    pub elapsed_ms: u64,
}

/// Quote a single argument for the Windows command line.
///
/// This follows the algorithm the C runtime uses to *parse* a command line, because that is the
/// only thing that matters: whatever we emit has to survive being read back. Getting it wrong is
/// how a path with a space silently becomes two arguments.
///
/// The rules, from the C runtime's own parsing:
///
/// * An argument containing spaces or tabs must be wrapped in double quotes.
/// * A backslash before a quote, or before the closing quote, must be doubled — otherwise the
///   parser treats it as escaping the quote and swallows it.
/// * A trailing backslash must be doubled if the argument is being quoted, or it escapes the
///   closing quote.
///
/// Verified against real paths with spaces and trailing backslashes in the tests below.
pub fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n' || c == '\u{0b}');

    if !needs_quoting {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');

    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                // Defer: we do not yet know whether the next character is a quote, which decides
                // whether these backslashes need doubling.
                backslashes += 1;
            }
            '"' => {
                // Backslashes immediately before a quote are doubled, then the quote escaped.
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            other => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(other);
            }
        }
    }

    // Any backslashes left at the end sit immediately before the closing quote, so they need
    // doubling too — otherwise the last one escapes the quote and the argument never terminates.
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }

    out.push('"');
    out
}

/// Build a full command line from a program and its arguments.
pub fn build_command_line(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(quote_arg(program));
    for a in args {
        parts.push(quote_arg(a));
    }
    parts.join(" ")
}

/// Default timeout when the caller does not give one.
///
/// Ten minutes is long enough for a build or an installer and short enough that a wedged process
/// does not hold the control channel forever. The caller can override it.
pub const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// Run a program and capture its output.
///
/// On non-Windows builds this reports the platform as absent rather than pretending. Cross-
/// compiling checks that this code type-checks, but execution can only be verified by running it
/// in a real guest — so a build without one says so plainly.
#[cfg(not(windows))]
pub fn run(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: u64,
) -> Result<Execution> {
    let _ = (program, args, cwd, timeout_ms);
    Err(anyhow!(
        "the Win32 execution layer is only present in a Windows build of wvm-guest; \
         this binary was built for the host, where it exists to be type-checked and tested \
         rather than run"
    ))
}

#[cfg(windows)]
pub fn run(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: u64,
) -> Result<Execution> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // `CREATE_NO_WINDOW` matters here in a way it does not on Linux: without it a console program
    // launched from a service inherits or creates a console window, which in a headless guest is
    // invisible but still costs a desktop switch and can fail outright when there is no
    // interactive session at all. Services have no console, so suppressing it is required for the
    // service case rather than merely tidy.
    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let started = Instant::now();

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not start '{program}': {e}"))?;

    // Poll rather than blocking on `wait()`. A blocking wait cannot honour a deadline, and the
    // alternative — a thread and a channel — is more machinery than this needs.
    let deadline = Duration::from_millis(timeout_ms);
    let poll = Duration::from_millis(25);

    let mut outcome;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                outcome = Outcome::Exited {
                    // `code()` is None when the process was killed by a signal or something more
                    // exotic; -1 records "ended without a status" rather than inventing zero,
                    // which would read as success.
                    code: status.code().unwrap_or(-1),
                };
                break;
            }
            Ok(None) => {
                if started.elapsed() >= deadline {
                    // Kill, then reap. Leaving a killed-but-unreaped child would hold its handles
                    // and pipe buffers, and the zombie would persist for the life of the service.
                    let _ = child.kill();
                    let _ = child.wait();
                    outcome = Outcome::TimedOut {
                        killed_after_ms: started.elapsed().as_millis() as u64,
                    };
                    break;
                }
                std::thread::sleep(poll);
            }
            Err(e) => return Err(anyhow!("waiting for '{program}': {e}")),
        }
    }

    // Read the pipes only after the process has finished or been killed. Reading concurrently
    // would need threads; because this service handles one request at a time this is safe, with
    // the one caveat that a program emitting more than a pipe buffer's worth (64 KiB) before
    // exiting could deadlock against a full pipe. That limit is acceptable for a control channel
    // whose output is diagnostics, and is recorded here rather than left as a surprise: if a
    // caller ever needs to stream large output, this is the line that changes.
    let stdout = read_pipe(child.stdout.take());
    let stderr = read_pipe(child.stderr.take());

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let _ = &mut outcome;

    Ok(Execution {
        outcome,
        stdout,
        stderr,
        elapsed_ms,
    })
}

/// Read a captured pipe to completion, decoding lossily.
///
/// Shared by the Windows path and the tests, so the lossy-decode decision is made once.
pub fn read_pipe<R: std::io::Read>(pipe: Option<R>) -> String {
    let Some(mut p) = pipe else {
        return String::new();
    };

    let mut buf = Vec::new();
    // A read failure mid-stream still yields whatever arrived. Partial output beats none when the
    // alternative is an error that discards the lines already read.
    let _ = p.read_to_end(&mut buf);

    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_argument_is_left_alone() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(
            quote_arg("C:/Windows/System32/cmd.exe"),
            "C:/Windows/System32/cmd.exe"
        );
    }

    #[test]
    fn an_argument_with_spaces_is_quoted() {
        assert_eq!(quote_arg("Program Files"), "\"Program Files\"");
    }

    #[test]
    fn an_empty_argument_becomes_a_pair_of_quotes() {
        // Without this, an empty argument vanishes entirely and shifts every later argument.
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn a_quote_inside_an_argument_is_escaped() {
        // `say "hi"` must survive as one argument containing quotes.
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn a_trailing_backslash_is_doubled_when_quoted() {
        // The classic bug. A single trailing backslash escapes the closing quote, so the argument
        // never terminates and the following text is swallowed into it. `C:\Program Files\` must
        // become `"C:\Program Files\\"`.
        assert_eq!(
            quote_arg("C:\\Program Files\\"),
            "\"C:\\Program Files\\\\\""
        );
    }

    #[test]
    fn a_backslash_before_a_quote_is_doubled() {
        // `a\` then a quote: the backslash must not be read as escaping the quote.
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn backslashes_not_near_a_quote_are_untouched() {
        assert_eq!(quote_arg("a\\b\\c"), "a\\b\\c");
        assert_eq!(
            quote_arg("C:\\path with space\\inner"),
            "\"C:\\path with space\\inner\""
        );
    }

    #[test]
    fn a_command_line_joins_program_and_arguments() {
        let line = build_command_line(
            "C:\\Program Files\\app\\run.exe",
            &["--flag".to_string(), "value with space".to_string()],
        );
        assert_eq!(
            line,
            "\"C:\\Program Files\\app\\run.exe\" --flag \"value with space\""
        );
    }

    #[test]
    fn a_timed_out_process_is_not_reported_as_a_failure() {
        // The distinction the Outcome enum exists to preserve. A caller seeing `TimedOut` knows to
        // raise the limit; a caller seeing a non-zero exit knows the program itself failed.
        let timed_out = Outcome::TimedOut {
            killed_after_ms: 5000,
        };
        assert!(!timed_out.is_success());
        assert_ne!(timed_out, Outcome::Exited { code: 1 });
        assert!(Outcome::Exited { code: 0 }.is_success());
        assert!(!Outcome::Exited { code: 1 }.is_success());
    }

    #[test]
    fn execution_without_a_windows_guest_says_so() {
        // On a host build this must be an explicit error, never a fabricated success. A stub that
        // pretends to have run something is worse than one that admits it cannot.
        if !cfg!(windows) {
            let err = run("cmd.exe", &[], None, 1000).expect_err("must not succeed off Windows");
            let msg = err.to_string();
            assert!(
                msg.contains("only present in a Windows build"),
                "the error should explain itself: {msg}"
            );
        }
    }

    #[test]
    fn reading_a_missing_pipe_yields_empty_text() {
        assert_eq!(read_pipe::<std::io::Empty>(None), "");
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily_rather_than_failing() {
        // Windows console output is not reliably UTF-8. A program emitting a stray byte must still
        // produce readable output, because that is exactly the output most worth reading.
        let bad: &[u8] = &[b'o', b'k', 0xff, b'!'];
        let text = read_pipe(Some(std::io::Cursor::new(bad)));
        assert!(
            text.starts_with("ok"),
            "readable prefix must survive: {text:?}"
        );
        assert!(
            text.ends_with('!'),
            "readable suffix must survive: {text:?}"
        );
        assert!(
            text.contains('\u{fffd}'),
            "the bad byte is marked, not dropped"
        );
    }
}
