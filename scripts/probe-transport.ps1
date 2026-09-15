# Transport probe: listen on the guest control port and report what arrives.
#
# Purpose: verify the whole guest-to-host path BEFORE building the Win32 service against it. If
# host-forwarded TCP does not reach a listener in the guest, every later step is built on sand.
#
# It listens on the configured guest port, accepts ONE connection, echoes a canned response, and
# prints what it received. Deliberately minimal: this tests the transport, not the protocol.
#
# Run INSIDE the guest, from an elevated PowerShell if the firewall needs a rule:
#   .\probe-transport.ps1
#
# Then from the HOST:
#   printf 'ping' | nc 127.0.0.1 48274
# or:
#   python3 -c "import socket;s=socket.create_connection(('127.0.0.1',48274));s.sendall(b'ping');print(s.recv(100))"

[CmdletBinding()]
param(
    # Must match guest_port in the host's VM config. The host forwards 127.0.0.1:48274 to this.
    [int]$Port = 48273,

    # How long to wait for one connection before giving up.
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'

Write-Host "wvm transport probe"
Write-Host ""

# --- firewall -----------------------------------------------------------------------------------
#
# Checked rather than assumed. A listening socket that the firewall drops looks EXACTLY like a
# socket that was never bound, from the host's point of view — both give a connection timeout. That
# ambiguity is worth eliminating before concluding anything about the transport.

$ruleName = "wvm probe ($Port)"
$existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
if (-not $existing) {
    Write-Host "  adding a temporary inbound rule for port $Port (local subnet only)..."
    New-NetFirewallRule `
        -DisplayName $ruleName `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort $Port `
        -RemoteAddress LocalSubnet `
        -Profile Any | Out-Null
} else {
    Write-Host "  firewall rule already present"
}
Write-Host ""

# --- listen -------------------------------------------------------------------------------------

$address = [System.Net.IPAddress]::Any
$listener = [System.Net.Sockets.TcpListener]::new($address, $Port)

try {
    $listener.Start()
} catch {
    Write-Error "could not bind $Port : $_`nIs something else already listening? Check: Get-NetTCPConnection -LocalPort $Port -State Listen"
    exit 1
}

# Report what we bound to, and whether it is reachable in principle. Binding to 0.0.0.0 is required:
# the QEMU forward arrives as an inbound connection on the guest's own interface, not on loopback.
Write-Host "  listening on 0.0.0.0:$Port"
Write-Host "  interfaces:"
Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -ne '127.0.0.1' } |
    ForEach-Object { Write-Host "    $($_.IPAddress)  ($($_.InterfaceAlias))" }
Write-Host ""
Write-Host "  PROVE local delivery first: in another elevated PowerShell run"
Write-Host "    Test-NetConnection -ComputerName 127.0.0.1 -Port $Port"
Write-Host ""
Write-Host "  Then from the HOST, run:"
Write-Host "    printf 'ping' | nc 127.0.0.1 $($Port + 1)"
Write-Host ""

# Accept ONE connection. A short poll loop rather than a blocking Accept, so the timeout is honoured
# and the script cannot hang forever waiting for a host that was never going to connect.
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$client = $null

while ((Get-Date) -lt $deadline -and -not $client) {
    if ($listener.Pending()) {
        $client = $listener.AcceptTcpClient()
    } else {
        Start-Sleep -Milliseconds 250
    }
}

if (-not $client) {
    Write-Host "  NO CONNECTION within $TimeoutSeconds seconds."
    Write-Host ""
    Write-Host "  That is a result, not a failure of the probe. What it means:"
    Write-Host "    * the host did not attempt a connection, or"
    Write-Host "    * host_port and guest_port do not line up between the two configs, or"
    Write-Host "    * the firewall dropped it (the rule above is scoped to LocalSubnet)"
    Write-Host ""
    Write-Host "  Check the forward on the HOST:  ss -tlnp | grep $(($Port + 1))"
    $listener.Stop()
    exit 2
}

# --- report -------------------------------------------------------------------------------------

$remote = $client.Client.RemoteEndPoint
Write-Host "  CONNECTED from $remote"

$stream = $client.GetStream()
$stream.ReadTimeout = 5000

$buffer = New-Object byte[] 1024
try {
    $read = $stream.Read($buffer, 0, $buffer.Length)
    if ($read -gt 0) {
        $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $read)
        Write-Host "  received $read byte(s): $text"
    } else {
        Write-Host "  connection opened but no data arrived"
    }
} catch {
    Write-Host "  read failed: $_"
}

# Reply, so the host side can confirm it is talking to US and not to some other service.
$reply = [System.Text.Encoding]::UTF8.GetBytes("wvm-probe-ok")
$stream.Write($reply, 0, $reply.Length)
$stream.Flush()
Write-Host "  replied: wvm-probe-ok"

$stream.Close()
$client.Close()
$listener.Stop()

Write-Host ""
Write-Host "  TRANSPORT VERIFIED: host -> guest reaches an arbitrary listener."
Write-Host ""

# --- cleanup ------------------------------------------------------------------------------------

Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
Write-Host "  removed the temporary firewall rule"
