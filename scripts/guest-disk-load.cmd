@echo off
REM Sustained disk I/O inside the guest, for one purpose only.
REM
REM The snapshot bugcheck is suspected to need writes IN FLIGHT when the device-state save runs —
REM the fault is a read at offset -8 from a null pointer in the Windows kernel, the classic signature
REM of an I/O request whose owner has gone away. If that is right, an idle guest will rarely crash
REM and any A/B test needs dozens of samples. Driving real I/O during the save should raise the rate
REM enough to test a fix in a handful of runs.
REM
REM It writes then deletes, in a loop, so it applies real pressure without filling the disk. It runs
REM until the caller's timeout fires; the exec layer's kill-on-close Job Object then takes the whole
REM tree, which is why this can be launched with a long timeout and left alone.
REM
REM Pass a loop count as %1 to make it end on its own; with no argument it runs until killed.

setlocal
set DIR=C:\ProgramData\wvm\staging
set FILE=%DIR%\load.tmp
set ROUNDS=%1
if "%ROUNDS%"=="" set ROUNDS=100000000

for /l %%r in (1,1,%ROUNDS%) do (
  for /l %%i in (1,1,500) do echo 0123456789ABCDEF0123456789ABCDEF %%i >> %FILE%
  del /f /q %FILE% >nul 2>&1
)
endlocal
