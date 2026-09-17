# Force a TRIM (retrim) across the system volume.
#
# WHY THIS IS THE NEXT TEST
#
# A guest disk declared with `-blockdev ...,discard=unmap` advertises discard to Windows. When
# Windows trims, QEMU unmaps the blocks — punching holes in the qcow2 while the disk is live.
#
# Every bugcheck on this machine post-dates that declaration, including one that happened before any
# snapshot had ever been saved. A TRIM is a rare, guest-initiated event, which fits a fault that
# shows up occasionally rather than constantly.
#
# And unlike a snapshot save, a TRIM can be TRIGGERED ON DEMAND. If a retrim crashes the guest, the
# hypothesis is confirmed in about a minute instead of an hour of A/B sampling, and the fix
# (removing `discard=unmap`) becomes a one-line change that can be verified the same way.
#
# -ReTrim asks the storage stack to re-issue trims for the whole volume. -Verbose reports progress
# so a silent no-op is distinguishable from a trim that ran.
#
# Written as a .ps1 and run with -File rather than passed on a command line, because quoting through
# the exec layer is a known trap in this project (D-014).

$ErrorActionPreference = "Continue"

Write-Output "=== volume before ==="
Get-Volume -DriveLetter C | Select-Object DriveLetter, FileSystemType, HealthStatus, SizeRemaining |
    Format-List

Write-Output "=== does the disk report TRIM as supported? ==="
# If Windows does not think the device supports trim, nothing below means anything.
$trim = Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" `
    -Name DisableDeleteNotify -ErrorAction SilentlyContinue
Write-Output ("  DisableDeleteNotify = {0}   (0 or absent means TRIM is ENABLED)" -f $trim.DisableDeleteNotify)

Write-Output ""
Write-Output "=== issuing retrim on C: ==="
$sw = [Diagnostics.Stopwatch]::StartNew()
try {
    Optimize-Volume -DriveLetter C -ReTrim -Verbose -ErrorAction Stop 4>&1 | ForEach-Object {
        Write-Output ("  " + $_.ToString())
    }
    Write-Output ("  retrim completed in {0:N1}s" -f $sw.Elapsed.TotalSeconds)
} catch {
    Write-Output ("  retrim FAILED: " + $_.Exception.Message)
}

Write-Output ""
Write-Output "=== volume after ==="
Get-Volume -DriveLetter C | Select-Object DriveLetter, SizeRemaining | Format-List

Write-Output "=== DUMP COUNT (if this differs from before, the guest bugchecked) ==="
Get-ChildItem C:\Windows\Minidump -Filter *.dmp -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty Name
Write-Output "=== RETRIM SCRIPT FINISHED ==="
