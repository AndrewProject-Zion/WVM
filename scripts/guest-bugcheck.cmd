@echo off
REM Extract the crash record from Windows' own event log.
REM
REM Shipped as a file rather than passed as a command line because this project has already been
REM bitten twice by quoting through the exec layer: quotes reach the process as part of the
REM argument, and `&&` does not survive at all. A script file has no quoting to get wrong.

echo === BugCheck (EventID 1001) ===
wevtutil qe System /q:"*[System[(EventID=1001)]]" /c:5 /rd:true /f:text
echo.
echo === Unexpected shutdown (EventID 6008) ===
wevtutil qe System /q:"*[System[(EventID=6008)]]" /c:3 /rd:true /f:text
echo.
echo === Disk / storage errors (EventID 7, 11, 51, 153) ===
wevtutil qe System /q:"*[System[(EventID=7 or EventID=11 or EventID=51)]]" /c:5 /rd:true /f:text
