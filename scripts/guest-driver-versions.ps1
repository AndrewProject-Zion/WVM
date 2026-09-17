# Which virtio drivers is the guest actually running, and how old are they?
#
# WHY: the guest bugchecks with 0x50 PAGE_FAULT_IN_NONPAGED_AREA at a fixed code offset whenever a
# snapshot saves device state, and a bare stop/cont does not reproduce it. The faulting module has
# not been named yet (the dumps are PAGEDU64 kernel dumps, not MDMP minidumps, so the module list is
# not trivially readable).
#
# The storage driver is the first thing to rule out, because it is the one device whose state the
# snapshot saves and the one component most likely to be version-mismatched against QEMU 11.
# A driver older than the hypervisor is a concrete, fixable cause; a current one is not.
#
# Written as a .ps1 and run with -File rather than passed as a command line, because quoting
# through the exec layer is a known trap in this project (D-014).

$ErrorActionPreference = "SilentlyContinue"

Write-Output "=== virtio driver files ==="
$names = @("viostor", "vioscsi", "netkvm", "vioserial", "viomem", "viosock", "balloon",
           "vioinput", "viorng", "viofs", "qxldod")
foreach ($n in $names) {
    $p = "C:\Windows\System32\drivers\$n.sys"
    if (Test-Path $p) {
        $item = Get-Item $p
        $v = $item.VersionInfo.FileVersion
        Write-Output ("  {0,-12} {1,-20} {2}" -f $n, $v, $item.LastWriteTime.ToString("yyyy-MM-dd"))
    }
}

Write-Output ""
Write-Output "=== installed virtio driver packages (pnputil) ==="
$pn = & pnputil /enum-drivers 2>$null
$block = @()
$capture = $false
foreach ($line in $pn) {
    if ($line -match "^Published Name|^Published:") { $block = @{}; $capture = $true }
    if ($capture) {
        $block[$line.Trim()] = $true
        if ($line -match "(?i)virtio|viostor|netkvm|vioscsi|balloon|vioserial") {
            Write-Output ("  " + $line.Trim())
        }
    }
}

Write-Output ""
Write-Output "=== the storage controller Windows sees ==="
Get-CimInstance Win32_DiskDrive | ForEach-Object {
    Write-Output ("  model={0}  interface={1}" -f $_.Model, $_.InterfaceType)
}

Write-Output ""
Write-Output "=== system ==="
$os = Get-CimInstance Win32_OperatingSystem
Write-Output ("  {0}  build {1}" -f $os.Caption, $os.BuildNumber)
Write-Output ("  memory visible: {0:N0} MB" -f ($os.TotalVisibleMemorySize / 1024))
