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
    # Where the cross-compiled binary is. Defaults to beside this script.
    [string]$Source = (Join-Path $PSScriptRoot 'wvm-guest.exe'),

    # Must match guest_port in the host's VM config, or the forward lands on nothing.
    [int]$Port = 48273,

    # Where the service binary lives once installed.
    [string]$InstallDir = 'C:\Program Files\wvm'
)

$ErrorActionPreference = 'Stop'
$serviceName = 'wvm-guest'

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

$binaryPath = "`"$target`" --bind 0.0.0.0:$Port"

if ($existing) {
    # Re-point an existing registration rather than trying to create a duplicate.
    & sc.exe config $serviceName binPath= $binaryPath start= auto | Out-Null
    Write-Host "  service     reconfigured"
} else {
    & sc.exe create $serviceName binPath= $binaryPath start= auto DisplayName= "WVM Guest Service" | Out-Null
    Write-Host "  service     created"
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

Write-Host ""
Write-Host "Installed. The service is registered but NOT started."
Write-Host ""
Write-Host "  start it:   Start-Service $serviceName"
Write-Host "  check it:   Get-Service $serviceName"
Write-Host "  its log:    Get-EventLog -LogName Application -Source $serviceName -Newest 20"
Write-Host "  remove it:  Stop-Service $serviceName; sc.exe delete $serviceName"
Write-Host ""
Write-Host "  listening on 0.0.0.0:$Port; the host reaches it via 127.0.0.1:<forward_port>"
Write-Host ""
Write-Host "Wiring if a connection is refused from the host:"
Write-Host "  1. is the service running?          Get-Service $serviceName"
Write-Host "  2. is it listening?                 Get-NetTCPConnection -LocalPort $Port -State Listen"
Write-Host "  3. does guest_port match the host?  the host forwards to this exact port"
