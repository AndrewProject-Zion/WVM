# What does Windows itself believe about this machine's internet connectivity?
#
# WHY: after Pi-hole was found to be blocking www.msftconnecttest.com, the guest's own connectivity
# verdict is the thing to read - not our curl. IPv4Connectivity is NCSI's answer: "Internet" means
# the probe passed, "LocalNetwork"/"NoTraffic" means Windows is showing the no-internet globe.
#
# Run as a file, not inline: quoting a PowerShell command through the exec layer mangles it (it
# echoed the command back rather than running it, which looks like success in a captured log).

$ErrorActionPreference = 'Stop'

foreach ($c in Get-NetConnectionProfile) {
    Write-Output "  adapter      : $($c.InterfaceAlias)"
    Write-Output "  network      : $($c.Name)"
    Write-Output "  category     : $($c.NetworkCategory)"
    Write-Output "  IPv4 state   : $($c.IPv4Connectivity)"
    Write-Output "  IPv6 state   : $($c.IPv6Connectivity)"
}

$k = 'HKLM:\SYSTEM\CurrentControlSet\Services\NlaSvc\Parameters\Internet'
$p = Get-ItemProperty $k
Write-Output "  NCSI probe   : http://$($p.ActiveWebProbeHost)$($p.ActiveWebProbePath)"
Write-Output "  NCSI expects : $($p.ActiveWebProbeContent)"

# The definitive live answer: ask NCSI directly whether it thinks there is internet.
Add-Type -AssemblyName System.Runtime.WindowsRuntime -ErrorAction SilentlyContinue
$ns = [Windows.Networking.Connectivity.NetworkInformation, Windows.Networking.Connectivity, ContentType=WindowsRuntime]
$profile = $ns::GetInternetConnectionProfile()
if ($null -eq $profile) {
    Write-Output "  NCSI verdict : NO PROFILE (no internet)"
} else {
    $level = $profile.GetNetworkConnectivityLevel()
    Write-Output "  NCSI verdict : $level"
}
