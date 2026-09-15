@echo off
REM Spawn a stubborn process hierarchy for the timeout-cleanup test.
REM
REM Why this exists: a caller timeout that kills only the direct child leaves that child's own
REM children running. Kill `cmd` and whatever IT started keeps going. Do that repeatedly and the
REM guest accumulates orphans until it exhausts handle/PID space or memory — which is how an agent
REM sandbox gets bricked by its own workload.
REM
REM Structure, deliberately awkward:
REM   - the PARENT outlives its children (so killing only it is visibly insufficient)
REM   - there are TWO grandchildren, so a single lucky kill cannot pass the test
REM   - the grandchildren survive far longer than any timeout under test
REM
REM WHY NOT `timeout /t 600`
REM
REM The obvious Windows stand-in for `sleep 600` is `timeout`, and it is a TRAP under this harness:
REM the guest runs every command with stdin redirected to null, and `timeout.exe` refuses to run in
REM that case —
REM
REM     ERROR: Input redirection is not supported, exiting the process immediately.
REM
REM So `timeout /t 600` exits in milliseconds, no grandchild ever exists, and a cleanup test counts
REM zero survivors and PASSES for the wrong reason. That is exactly what happened on the first run
REM of scripts/test-timeout-tree-kill.py: the parent completed in 82ms and the suite reported success.
REM
REM `ping -n N 127.0.0.1` is the delay that works with redirected stdin — it is a real child process,
REM so it appears in the task list and must be cleaned up explicitly, which is what the test needs.
REM `N` counts packets, so N=200 is roughly 200 seconds.

echo parent started

start /b cmd /c "ping -n 200 127.0.0.1 >nul"
start /b cmd /c "ping -n 200 127.0.0.1 >nul"

REM The parent waits too, so it is still alive when a caller timeout fires.
ping -n 190 127.0.0.1 >nul

echo parent finished
