@echo off
REM Start the guest in CONSOLE mode (not as a service) with a short request deadline.
REM
REM Why: the production deadline is fifteen minutes, which cannot be tested by waiting. The guest
REM reads WVM_REQUEST_DEADLINE_SECS so the failure path is reachable, and this starts a console
REM instance with it set low.
REM
REM Console mode rather than the service for two reasons: the environment variable is trivial to
REM pass, and console output is visible, which makes a failure diagnosable rather than silent.
REM
REM Stop the service first — it holds the same port and the same staging directory.
REM
REM Usage (from inside the guest):
REM    C:\ProgramData\wvm\staging\deadline-guest.cmd

set WVM_REQUEST_DEADLINE_SECS=5

echo starting the guest in console mode with a 5 second request deadline
"C:\Program Files\wvm\wvm-guest.exe" --port 48273
