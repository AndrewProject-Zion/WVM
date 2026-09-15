@echo off
REM Swap the running wvm-guest.exe for a newer build, from inside the guest.
REM
REM Why this exists as a file rather than a Run-dialog one-liner: the Run dialog field scrolls and
REM guillemets/backslashes fight the emulated keymap, and a console opened from Run is invisible to
REM an agent watching only the framebuffer. A script gets typed ONCE by a short command, and its
REM output goes to a log that can be pulled back over the control channel or read from the host.
REM
REM The sequence matters, and it is the bug that broke the first attempt:
REM   the SCM restarts the service within ~5 seconds of a kill (failure actions are
REM   restart/5000/restart/10000/restart/30000), so a copy issued after walking through an elevation
REM   prompt races the restart and fails with "being used by another process". Stop, copy and start
REM   must happen back to back, with no human in the middle.

setlocal

set NEWEXE=%TEMP%\wvm-guest-new.exe
set TARGET=C:\Program Files\wvm\wvm-guest.exe
set LOG=%USERPROFILE%\wvm\swap.log

REM Redirect everything to the log from here on. `>nul 2>&1` on each command would hide failures.
echo ==== swap started %DATE% %TIME% ==== > "%LOG%"

echo [1/6] fetching the new binary >> "%LOG%"
curl -sS -o "%NEWEXE%" http://10.0.2.2:8899/wvm-guest.exe >> "%LOG%" 2>&1
if errorlevel 1 goto :failed

for %%A in ("%NEWEXE%") do echo       fetched %%~zA bytes >> "%LOG%"

echo [2/6] stopping the service >> "%LOG%"
sc stop wvm-guest >> "%LOG%" 2>&1
REM Give the SCM a moment, then kill whatever is still holding the file. The old build has no
REM control handler, so `sc stop` returns without stopping it? that is exactly what happened during
REM development and it blocked the copy.
ping -n 3 127.0.0.1 >nul
taskkill /f /im wvm-guest.exe >> "%LOG%" 2>&1
ping -n 2 127.0.0.1 >nul

echo [3/6] copying into place >> "%LOG%"
copy /y "%NEWEXE%" "%TARGET%" >> "%LOG%" 2>&1
if errorlevel 1 goto :failed

echo [4/6] confirming the copy >> "%LOG%"
for %%A in ("%TARGET%") do echo       target is now %%~zA bytes >> "%LOG%"
for %%A in ("%NEWEXE%") do echo       source was %%~zA bytes >> "%LOG%"

echo [5/6] starting the service >> "%LOG%"
sc start wvm-guest >> "%LOG%" 2>&1
ping -n 4 127.0.0.1 >nul

echo [6/6] service state >> "%LOG%"
sc query wvm-guest >> "%LOG%" 2>&1

echo ==== swap finished %DATE% %TIME% ==== >> "%LOG%"

REM Surface the log in the console too, so it is readable if a human is watching the framebuffer.
type "%LOG%"
exit /b 0

:failed
echo FAILED at the step above >> "%LOG%"
type "%LOG%"
exit /b 1
