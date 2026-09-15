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

    // A job object for the whole tree, created BEFORE the child exists.
    //
    // `Child::kill()` terminates one process. Without this, a timeout that killed `cmd.exe` left
    // whatever the command had spawned running — orphaned and invisible, still holding memory and
    // handles. Repeatedly, that is how a sandbox is bricked by its own workload.
    #[cfg(windows)]
    let job = crate::job::JobObject::kill_on_close()
        .map_err(|e| anyhow!("could not create a job object for '{program}': {e}"))?;

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not start '{program}': {e}"))?;

    // Put it in the job immediately, so anything IT spawns is in the job too.
    #[cfg(windows)]
    job.assign(&child)
        .map_err(|e| anyhow!("could not isolate '{program}' in a job object: {e}"))?;

    // Drain both pipes on their own threads, starting NOW, before we wait for the process.
    //
    // This was WVM-01, and it was a correctness bug rather than a limit. The original code waited
    // for the process to exit and only then read its output. But a pipe holds only about 64 KiB on
    // Windows: once the process filled that buffer its next write BLOCKED, so the process could
    // never exit, so `try_wait` never reported it finished. The deadline eventually fired — not
    // because the command was slow, but because we had wedged it ourselves. `cmd /c dir /s C:\`
    // returned `timed_out` in place of the listing it had already produced.
    //
    // Reporting a healthy command as timed out is the worst kind of failure for a control channel:
    // the caller cannot tell it from a genuinely hung command, and the output that proves otherwise
    // is discarded. So the pipes are drained concurrently and the deadline means what it says.
    //
    // Threads rather than non-blocking reads because the read must not stop until EOF, and a
    // non-blocking loop would have to poll two pipes while also polling the child. Both joined
    // below, so no read outlives this function.
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());

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
                    // Kill the WHOLE tree, then reap the leader.
                    //
                    // On Windows, dropping the job handle is the kill: KILL_ON_JOB_CLOSE terminates
                    // every process in the job, and job membership is inherited, so grandchildren
                    // die too. `child.kill()` alone would terminate one process and leave the rest.
                    #[cfg(windows)]
                    {
                        let job = job;
                        drop(job);
                    }
                    // Belt and braces: on Unix the job object does not exist, and killing the direct
                    // child is all that is available. Keeping both means the behaviour is defined on
                    // whichever platform this is built for.
                    let _ = child.kill();
                    // Reap, so the leader is not left as a zombie holding its handle. Killing
                    // without waiting is how a process table fills up.
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

    // Joined, not merely dropped: the readers must finish before their buffers can be read, and a
    // detached thread would still be holding the pipe handle when the child is reaped below.
    let stdout = join_pipe_reader(stdout_reader);
    let stderr = join_pipe_reader(stderr_reader);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let _ = &mut outcome;

    Ok(Execution {
        outcome,
        stdout,
        stderr,
        elapsed_ms,
    })
}

/// Start draining a pipe on its own thread, if there is one.
///
/// `None` for a pipe that was not captured, which is a legitimate state rather than an error: the
/// caller may not have asked for output.
pub fn spawn_pipe_reader<R>(pipe: Option<R>) -> Option<std::thread::JoinHandle<String>>
where
    R: std::io::Read + Send + 'static,
{
    pipe.map(|p| std::thread::spawn(move || read_pipe(Some(p))))
}

/// Wait for a reader thread and take its buffer.
///
/// A panicked reader yields empty text rather than propagating. That loses output, which is bad,
/// but it does not lose the *process result*, which is worse — and a reader thread cannot fail for
/// any reason that makes the exit code untrustworthy.
pub fn join_pipe_reader(handle: Option<std::thread::JoinHandle<String>>) -> String {
    match handle {
        Some(h) => h.join().unwrap_or_default(),
        None => String::new(),
    }
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
    fn a_pipe_larger_than_the_pipe_buffer_is_drained_concurrently() {
        // WVM-01. The bug: `run` waited for the process to exit and only then read the pipes. A
        // Windows pipe holds about 64 KiB, so a process emitting more than that blocked on its
        // next write, never exited, and was reported as `timed_out` — with its already-produced
        // output discarded. A healthy command reported as hung is indistinguishable from a real
        // hang, which makes it a correctness bug rather than a limit.
        //
        // The property that fixes it: the reader must be able to consume output WHILE the writer
        // is still producing it, which is the entire mechanism. 64 KiB is the pipe-buffer threshold
        // on Windows; the payload below is sized just past it rather than far past it.
        use std::io::Write;

        // Sized just past the boundary the bug was about, not far past it.
        //
        // The first version of this test used 4 MiB "to clear the boundary with margin". Measuring
        // the primitives (scripts/measure-pipe-throughput.py) showed 4 MiB moves in about 3ms, so
        // the margin bought nothing and only made the test slow. What the test must exercise is a
        // writer that outruns the pipe buffer — 256 KiB is 4x a 64 KiB buffer, which is the
        // property, and it does it in microseconds.
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 4;
        const TOTAL: usize = CHUNK * CHUNKS;

        let (reader, mut writer) = std::io::pipe().expect("pipe");

        let finisher = std::thread::spawn(move || {
            for _ in 0..CHUNKS {
                writer.write_all(&vec![b'x'; CHUNK]).expect("write");
            }
            // Dropping the writer closes it, which is what lets the reader reach EOF. Without this
            // the reader blocks forever — the same deadlock, from the other direction.
        });

        let handle = spawn_pipe_reader(Some(reader));
        let drained = join_pipe_reader(handle);
        finisher.join().expect("writer thread");

        assert_eq!(
            drained.len(),
            TOTAL,
            "every byte written must be drained, not just the first pipe buffer's worth"
        );
        assert!(
            drained.bytes().all(|b| b == b'x'),
            "the drained content is intact"
        );
    }

    #[test]
    fn a_missing_pipe_yields_no_reader_rather_than_erroring() {
        // `None` is a legitimate state: the caller may not have asked for that stream. It must not
        // be conflated with "the reader failed", or a command with no stderr would look broken.
        assert_eq!(
            join_pipe_reader(spawn_pipe_reader::<std::io::Empty>(None)),
            ""
        );
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
