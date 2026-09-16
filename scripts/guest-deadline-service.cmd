@echo off
REM Run the guest service with a SHORT request deadline, for testing the backstop.
REM
REM WHY THIS IS DONE THIS WAY
REM
REM The deadline lives in WVM_REQUEST_DEADLINE_SECS, and the process that serves the control port is
REM the Windows SERVICE. An earlier attempt started a console instance with the variable set and
REM assumed it would take over the port — it did not. The service kept ownership, so every
REM measurement was of a process running the fifteen-minute default. Five attempts went into testing
REM the wrong process.
REM
REM A service inherits its environment from the SCM, which inherits from the machine at boot, so the
REM variable has to be set with `setx /M` and the service restarted. `setx /M` needs elevation.
REM
REM This script does that, and prints what it did. It is deliberately explicit rather than clever:
REM the last version of this test was silent about which process it had actually measured.
REM
REM Usage, from an ELEVATED prompt inside the guest:
REM     C:\ProgramData\wvm\staging\deadline-service.cmd 5
REM and to undo it:
REM     C:\ProgramData\wvm\staging\deadline-service.cmd clear

setlocal

if "%~1"=="" (
    echo usage: %~nx0 ^<seconds^>   or   %~nx0 clear
    exit /b 2
)

if /i "%~1"=="clear" (
    echo clearing the deadline override
    setx /M WVM_REQUEST_DEADLINE_SECS ""
    echo restarting the service to pick up the cleared environment
    sc stop wvm-guest
    timeout /t 3 /nobreak >nul 2>&1
    sc start wvm-guest
    echo done. The service is back on the default deadline.
    exit /b 0
)

echo setting the machine-wide request deadline to %~1 seconds
setx /M WVM_REQUEST_DEADLINE_SECS %~1

echo restarting the service so it inherits the new environment
sc stop wvm-guest
REM sc stop can fail with 1061 while a request is in flight. Not fatal: the restart below will
REM fail loudly if the service is still holding the port.
timeout /t 3 /nobreak >nul 2>&1
sc start wvm-guest

echo.
echo the service is now running with a %~1 second deadline.
echo remember to run this script with 'clear' afterwards.
