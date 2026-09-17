@echo off
REM Switch the guest to SMALL memory dumps, so the next crash produces a parseable minidump.
REM
REM WHY: the guest bugchecks 0x50 at a fixed code offset whenever a snapshot saves device state. The
REM four dumps we have are PAGEDU64 kernel dumps, whose loaded-module list lives in dumped kernel
REM memory behind a virtual-address translation — not something to parse casually.
REM
REM A small memory dump (CrashDumpEnabled=3) writes an MDMP minidump to C:\Windows\Minidump that
REM carries the stop code, the parameters, the loaded driver list WITH base addresses, and the
REM faulting thread's stack. That is exactly what is needed to NAME the driver at the faulting
REM address, in 256 KB, with no debugger.
REM
REM CrashDumpEnabled: 0=none 1=complete 2=kernel 3=small 7=automatic
REM
REM Written as a file and executed rather than passed on a command line: quoting through the exec
REM layer is a known trap in this project (D-014), and `reg` is especially unforgiving of it.

echo === current setting ===
reg query "HKLM\SYSTEM\CurrentControlSet\Control\CrashControl" /v CrashDumpEnabled
reg query "HKLM\SYSTEM\CurrentControlSet\Control\CrashControl" /v DumpFile
reg query "HKLM\SYSTEM\CurrentControlSet\Control\CrashControl" /v MinidumpDir

echo.
echo === switching to small memory dumps (3) ===
reg add "HKLM\SYSTEM\CurrentControlSet\Control\CrashControl" /v CrashDumpEnabled /t REG_DWORD /d 3 /f

echo.
echo === confirm ===
reg query "HKLM\SYSTEM\CurrentControlSet\Control\CrashControl" /v CrashDumpEnabled

echo.
echo === existing dumps still on disk ===
dir /b C:\Windows\Minidump
