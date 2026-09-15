#!/usr/bin/env pwsh
# Parse-check the guest service installer without running it.
#
# The installer only runs inside a Windows guest, so a syntax error would otherwise be discovered
# at the worst possible moment — as a half-completed install with the service already stopped.
# pwsh on the host is enough to catch that.

$ErrorActionPreference = 'Stop'

$target = Join-Path $PSScriptRoot 'install-guest-service.ps1'
if (-not (Test-Path $target)) {
    Write-Host "not found: $target"
    exit 1
}

$tokens = $null
$errors = $null
$null = [System.Management.Automation.Language.Parser]::ParseFile($target, [ref]$tokens, [ref]$errors)

if ($errors -and $errors.Count -gt 0) {
    Write-Host "PARSE ERRORS in $target"
    foreach ($e in $errors) {
        Write-Host ("  line {0}: {1}" -f $e.Extent.StartLineNumber, $e.Message)
    }
    exit 1
}

Write-Host "syntax OK ($($tokens.Count) tokens)"
exit 0
