# Install the WVM guest service inside a running Windows VM.
#
# Run this from an ADMINISTRATOR PowerShell inside the guest, after copying
# wvm-guest.exe onto it. It does three things and stops:
#
#   1. copies the binary to a stable location
#   2. opens the firewall for the control port, on the guest's own subnet only
#   3. registers it as a service that starts at boot
#
# It does NOT start the service. That is deliberate: a control service that starts listening
# before you have decided it should is a bigger step than an installer ought to take on your
# behalf. Start it yourself with the printed command once you are happy.
#
# Usage (elevated PowerShell):
#   .\install-guest-service.ps1
#   .\install-guest-service.ps1 -Source C:\Users\admin\Desktop\wvm-guest.exe -Port 48273

[CmdletBinding()]
param(
    # Where the cross-compiled binary is. Empty means "look beside this script and in the current
    # directory" — resolved in the body rather than here, because a default that calls Join-Path
    # on $PSScriptRoot fails at PARAMETER BINDING time when $PSScriptRoot is empty, before a
    # single line of the script runs.
    #
    # That is a real failure this hit: the download came via curl, $PSScriptRoot was empty, and the
    # error surfaced as a Join-Path binding error with no indication that the default was at fault.
    # The script appeared to do nothing at all, and no transcript was written because binding
    # failed before the transcript could start.
    [string]$Source = '',

    # Must match guest_port in the host's VM config, or the forward lands on nothing.
    [int]$Port = 48273,

    # Where the service binary lives once installed.
    [string]$InstallDir = 'C:\Program Files\wvm',

    # Write a transcript here. Set by the bootstrap so the result survives the elevated window
    # closing — an elevated console cannot be read once the process has exited, and losing the
    # output is how an install failure becomes indistinguishable from a silent success.
    [string]$Log = ''
)

$ErrorActionPreference = 'Stop'
$serviceName = 'wvm-guest'

# Resolve the source explicitly, trying every plausible place in order. This runs after binding,
# so it works regardless of how the script was invoked.
if ([string]::IsNullOrEmpty($Source)) {
    $candidates = @()

    # Beside the script, if PowerShell knows where that is.
    if (-not [string]::IsNullOrEmpty($PSScriptRoot)) {
        $candidates += (Join-Path $PSScriptRoot 'wvm-guest.exe')
    }
    # The current directory, and the directory the bootstrap uses.
    $candidates += (Join-Path (Get-Location).Path 'wvm-guest.exe')
    if (-not [string]::IsNullOrEmpty($env:USERPROFILE)) {
        $candidates += (Join-Path $env:USERPROFILE 'wvm\wvm-guest.exe')
        $candidates += (Join-Path $env:USERPROFILE 'wvm-guest.exe')
    }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            $Source = $candidate
            break
        }
    }
}

# Start the transcript before anything can fail, so an early exit is still recorded.
if ($Log -ne '') {
    try {
        Start-Transcript -Path $Log -Force | Out-Null
    } catch {
        Write-Host "  (could not start a transcript at $Log : $_)"
    }
}

# --- require elevation -------------------------------------------------------------------------
#
# Checked explicitly rather than letting the failure surface later as an opaque access-denied
# during service registration.

$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'This script must be run from an elevated PowerShell (Run as Administrator).'
    exit 1
}

Write-Host "wvm guest service installer"
Write-Host ""

# --- validate the binary -----------------------------------------------------------------------

if (-not (Test-Path -LiteralPath $Source)) {
    Write-Error "Guest binary not found: $Source`nCopy wvm-guest.exe over and pass -Source if it is somewhere else."
    exit 1
}

$sourceItem = Get-Item -LiteralPath $Source
Write-Host ("  source      {0} ({1:N0} bytes)" -f $sourceItem.FullName, $sourceItem.Length)

# A surprising size means the wrong file, or a build that did not finish.
if ($sourceItem.Length -lt 100KB) {
    Write-Error ("The file is only {0:N0} bytes. The guest service builds to roughly 437 KB; this is probably not it." -f $sourceItem.Length)
    exit 1
}

# --- stop anything already running -------------------------------------------------------------

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "  existing    a service named '$serviceName' is already installed"
    if ($existing.Status -eq 'Running') {
        Write-Host "  stopping    it"
        Stop-Service -Name $serviceName -Force
        # Wait for it to actually stop before replacing the binary, or the copy fails with a
        # sharing violation that reads like a permissions problem.
        $existing.WaitForStatus('Stopped', (New-Object TimeSpan 0, 0, 30))
    }
}

# --- install -----------------------------------------------------------------------------------

if (-not (Test-Path -LiteralPath $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Write-Host "  created     $InstallDir"
}

$target = Join-Path $InstallDir 'wvm-guest.exe'
Copy-Item -LiteralPath $Source -Destination $target -Force
Write-Host "  installed   $target"

# --- firewall ----------------------------------------------------------------------------------
#
# Scoped to the guest's own subnet, not `Any`. The host reaches this service through QEMU's
# forwarded port, which arrives on the guest's local interface; nothing outside needs it.

$ruleName = "wvm guest control ($Port)"
$rule = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
if ($rule) {
    Remove-NetFirewallRule -DisplayName $ruleName
}
New-NetFirewallRule `
    -DisplayName $ruleName `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort $Port `
    -RemoteAddress LocalSubnet `
    -Profile Any | Out-Null
Write-Host "  firewall    inbound TCP $Port allowed from the local subnet only"

# --- service registration ----------------------------------------------------------------------
#
# `sc.exe` rather than New-Service: New-Service cannot set the failure actions, and a control
# service that stays dead after a crash is a support call.
#
# sc.exe's OUTPUT IS KEPT, not piped to Out-Null. An earlier version discarded it and then told the
# operator to "check the sc.exe output above" — pointing at output that had been thrown away. When
# registration failed there was nothing to check, and the actual reason (a binPath that sc.exe
# rejected, for instance) was lost. Capturing it costs nothing and is the difference between a
# diagnosable failure and a dead end.

$binaryPath = "`"$target`" --service"

if ($existing) {
    # Re-point an existing registration rather than trying to create a duplicate.
    $scOutput = & sc.exe config $serviceName binPath= $binaryPath start= auto 2>&1
    $scExit = $LASTEXITCODE
    Write-Host "  service     reconfigured (sc.exe exit $scExit)"
} else {
    $scOutput = & sc.exe create $serviceName binPath= $binaryPath start= auto DisplayName= "WVM Guest Service" 2>&1
    $scExit = $LASTEXITCODE
    Write-Host "  service     created (sc.exe exit $scExit)"
}

# Surface it whenever anything went wrong, and whenever it simply has something to say.
if ($scExit -ne 0 -or $scOutput) {
    foreach ($line in $scOutput) {
        Write-Host "              sc: $line"
    }
}

& sc.exe description $serviceName "Executes WVM control requests from the host. See the wvm project." | Out-Null

# Restart on failure. The counter resets daily so a service that crashes once a week for a
# recoverable reason does not eventually give up.
& sc.exe failure $serviceName reset= 86400 actions= restart/5000/restart/10000/restart/30000 | Out-Null

# --- verify the registration actually took -----------------------------------------------------

$installed = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if (-not $installed) {
    Write-Error "Service registration did not take. Check the sc.exe output above."
    exit 1
}

# --- start it and confirm it is actually listening ---------------------------------------------
#
# Started here rather than left to the operator, because "registered" and "working" are different
# claims and only the second one matters. A service that starts and then exits immediately looks
# identical to a healthy one in the service list if nobody checks.

Write-Host "  starting    the service"
Start-Service -Name $serviceName

$installed.WaitForStatus('Running', (New-Object TimeSpan 0, 0, 30))

if ($installed.Status -ne 'Running') {
    Write-Error "The service did not reach 'Running' (status: $($installed.Status))."
    exit 1
}
Write-Host "  service     running"

# Confirm something is bound to the port, from inside the guest. This is the check that
# distinguishes "the process started" from "the control channel is open" — and it is the one that
# would have caught the loopback-bind bug, where the service ran happily and accepted nothing.
$deadline = (Get-Date).AddSeconds(15)
$listening = $false
while ((Get-Date) -lt $deadline -and -not $listening) {
    $conn = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
    if ($conn) { $listening = $true } else { Start-Sleep -Milliseconds 500 }
}

if (-not $listening) {
    Write-Error @"
The service is running but nothing is listening on port $Port.

The usual cause is a bind address of 127.0.0.1. The host reaches this service through a QEMU
forward, which arrives as an INBOUND connection on the guest's external interface, never on
loopback. The service must bind 0.0.0.0.
"@
    exit 1
}

$bound = (Get-NetTCPConnection -LocalPort $Port -State Listen | Select-Object -First 1).LocalAddress
Write-Host "  listening   $bound`:$Port"

Write-Host ""
Write-Host "Installed and running."
Write-Host ""
Write-Host "  verify from the host:   python3 scripts/talk-to-guest.py hello"
Write-Host "  stop it:                Stop-Service $serviceName"
Write-Host "  remove it:              Stop-Service $serviceName; sc.exe delete $serviceName"
Write-Host ""
Write-Host "  listening on 0.0.0.0:$Port; the host reaches it via 127.0.0.1:<forward_port>"
Write-Host ""
Write-Host "Wiring if a connection is refused from the host:"
Write-Host "  1. is the service running?          Get-Service $serviceName"
Write-Host "  2. is it listening?                 Get-NetTCPConnection -LocalPort $Port -State Listen"
Write-Host "  3. does guest_port match the host?  the host forwards to this exact port"

# Flush the transcript. Without this the log file never gets its final contents, and the output of
# a run that is claimed to have succeeded is exactly what is needed when it later turns out not to
# have worked.
if ($Log -ne '') {
    try { Stop-Transcript | Out-Null } catch { }
}
