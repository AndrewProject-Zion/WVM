@echo off
REM One-shot bootstrap for the WVM guest service.
REM
REM Why this exists as a file rather than a typed command:
REM
REM Typing into the guest console is viable for short commands and unreliable past roughly 110
REM characters, and nested quoting through `Start-Process -ArgumentList` is fragile even then. The
REM guest CAN pull from the host over HTTP reliably (a 505 KB binary downloaded cleanly), so the
REM robust pattern is: put the work in a script, download it once, then type a short command that
REM runs it. This file is that script.
REM
REM Run it by double-clicking, from the Startup folder, or with:
REM     inst.cmd
REM
REM It downloads the installer and the service binary, then runs the installer ELEVATED. The UAC
REM prompt it raises is the single unavoidable human step for a service install.

setlocal

set HOST=http://10.0.2.2:8899
set DEST=%USERPROFILE%\wvm
set LOG=%DEST%\install.log

echo.
echo   WVM guest service bootstrap
echo   ---------------------------
echo.

if not exist "%DEST%" mkdir "%DEST%"
cd /d "%DEST%"

echo   fetching the installer and the service binary...
curl -s -o inst.ps1        %HOST%/install-guest-service.ps1
curl -s -o wvm-guest.exe   %HOST%/wvm-guest.exe

if not exist inst.ps1 (
    echo   FAILED: could not fetch inst.ps1
    pause
    exit /b 1
)
if not exist wvm-guest.exe (
    echo   FAILED: could not fetch wvm-guest.exe
    pause
    exit /b 1
)

for %%F in (inst.ps1 wvm-guest.exe) do echo     %%F  %%~zF bytes

echo.
echo   running the installer elevated.
echo   Accept the User Account Control prompt to continue.
echo.

REM `Start-Process -Verb RunAs` is the elevation. The child runs the installer with its own
REM transcript redirected to a file, so the result is readable even after the window closes --
REM an elevated window's console cannot be read once it has exited, and its output was being lost.
powershell -NoProfile -Command "Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','%DEST%\inst.ps1','-Source','%DEST%\wvm-guest.exe'"

echo.
echo   install finished. log:
echo     %LOG%
echo.
if exist "%LOG%" (
    type "%LOG%"
) else (
    echo   (no log written - check the elevated window for errors^)
)

echo.
pause
