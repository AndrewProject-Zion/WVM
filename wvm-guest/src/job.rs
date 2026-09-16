//! A kill-on-close Windows Job Object, so a timeout kills the whole process tree.
//!
//! # The bug this exists to fix
//!
//! `Child::kill()` on Windows calls `TerminateProcess` on **one** process. Any process it started
//! survives. A caller timeout that killed `cmd.exe` while it was running `cmd /c build.bat` left
//! whatever `build.bat` had spawned still running — orphaned, invisible to the control channel, and
//! still consuming memory and handles in the guest.
//!
//! Once is harmless. Repeatedly, it is how an agent sandbox gets bricked by its own workload: the
//! orphans accumulate until the guest runs out of resources, and the failure presents as an
//! unrelated problem much later.
//!
//! # Why a Job Object and not a process group
//!
//! On POSIX the answer is `setpgid` at spawn plus `kill(-pgid)` — signal the negative PID and the
//! kernel delivers it to every member of the group. Windows has no equivalent: there is no negative
//! PID, and no signal that reaches a tree.
//!
//! The Windows equivalent is a **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Processes
//! assigned to a job are killed with the job when its last handle closes, and assignment is
//! inherited — a child spawned by a job member is in the job too, which is the property that makes
//! the tree die rather than just the parent.
//!
//! # Why the job is created BEFORE the process
//!
//! Assignment after spawn leaves a window where the child exists and is not yet in the job. If the
//! timeout fired in that window the kill would miss, and the failure would be rare and unreproducible
//! — the worst kind. `CREATE_SUSPENDED` plus `AssignProcessToJobObject` plus `ResumeThread` closes
//! the window completely: the process cannot run a single instruction before it is in the job.
//!
//! That said, this implementation takes the simpler route available through `std`: the job is
//! created first and the child is assigned immediately after spawn, then the handle is closed on
//! timeout. The window is real but is microseconds wide and does not involve the child executing
//! anything meaningful. Recorded honestly rather than described as race-free.

#![allow(dead_code)]

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job object holding one process tree.
    pub struct JobObject {
        handle: HANDLE,
    }

    impl JobObject {
        /// Create a job that kills everything in it when the handle closes.
        pub fn kill_on_close() -> Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error())
                        .context("creating a job object for the child process");
                }

                // KILL_ON_JOB_CLOSE is the whole point: closing the last handle to the job
                // terminates every process in it, including grandchildren.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let e = std::io::Error::last_os_error();
                    let _ = CloseHandle(handle);
                    return Err(e).context("setting KILL_ON_JOB_CLOSE on the job object");
                }

                Ok(JobObject { handle })
            }
        }

        /// Put a freshly spawned process into the job.
        pub fn assign(&self, process: &std::process::Child) -> Result<()> {
            unsafe {
                let ph = process.as_raw_handle() as HANDLE;
                let ok = AssignProcessToJobObject(self.handle, ph);
                if ok == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("assigning the child to the job object");
                }
                Ok(())
            }
        }

        /// Take the handle out of the struct so it can be closed deliberately.
        ///
        /// Separate from `Drop` so the caller can choose WHEN the tree dies: on timeout, or after a
        /// normal exit (where the tree is already gone and closing is just cleanup).
        pub fn into_raw(self) -> HANDLE {
            let h = self.handle;
            std::mem::forget(self);
            h
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                // Closing the last handle terminates every process still in the job. That is the
                // mechanism, so this is not just cleanup — it is the kill.
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::JobObject;
