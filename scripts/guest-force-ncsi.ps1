# Force NCSI to re-evaluate its "do I have internet?" verdict, and record the answer to a FILE.
#
# WHY A FILE
#
# The documented way to make NCSI re-probe is a network change event, and the surest one is bouncing
# the adapter. That drops the TCP connection the host uses to talk to this service - so the command
# would produce no reply and the host would see a timeout, which is indistinguishable from a crash.
# Writing the result to disk means the answer can be read afterwards on a fresh connection, instead
# of being lost with the connection that produced it.
#
# CONTEXT: Pi-hole was blocking www.msftconnecttest.com (matched by a smart-TV blocklist's
# `||www.msftconnecttest.com^` rule), so NCSI's probe got 0.0.0.0 and Windows reported "No internet"
# while DNS, ICMP and HTTPS all worked. Pi-hole now allows the exact FQDN; this asks Windows to
# notice.

$ErrorActionPreference = 'Continue'
$out = 'C:\ProgramData\wvm\staging\ncsi-after.txt'
Remove-Item $out -ErrorAction SilentlyContinue

function Get-NcsiLevel {
    $ns = [Windows.Networking.Connectivity.NetworkInformation, Windows.Networking.Connectivity, ContentType=WindowsRuntime]
    $p = $ns::GetInternetConnectionProfile()
    if ($null -eq $p) { return 'NoProfile' }
    return $p.GetNetworkConnectivityLevel().ToString()
}

"start : $(Get-NcsiLevel)" | Out-File $out

# Attempt 1 — the cheap one. Restarting the service that owns NCSI sometimes re-probes without
# touching the network at all.
"try   : restarting NlaSvc" | Out-File $out -Append
Restart-Service -Name nlasvc -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 25
"after1: $(Get-NcsiLevel)" | Out-File $out -Append

if ((Get-NcsiLevel) -eq 'Internet') {
    "done  : NlaSvc restart was enough" | Out-File $out -Append
    exit 0
}

# Attempt 2 — bounce the adapter. This is the reliable trigger, and the reason this script writes a
# file instead of returning.
"try   : bouncing the adapter" | Out-File $out -Append
Restart-NetAdapter -Name 'Ethernet' -Confirm:$false -ErrorAction SilentlyContinue
Start-Sleep -Seconds 30
"after2: $(Get-NcsiLevel)" | Out-File $out -Append
"done  : adapter bounced" | Out-File $out -Append
