param([int]$Seconds=8)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$d="$env:USERPROFILE\dev\latch6-field-final-20261114"
$binary="$env:USERPROFILE\dev\latch6-field-20261114\target\debug\latch-field-viewer.exe"
$name='Latch6FieldPublicRelayDeny20261114'
try {
    New-NetFirewallRule -Name $name -DisplayName 'Latch6 isolated viewer relay outage' -Direction Outbound -Action Block -Protocol TCP -RemoteAddress 54.116.93.176 -RemotePort 443 -Program $binary -Profile Any | Out-Null
    [pscustomobject]@{ utc=(Get-Date).ToUniversalTime().ToString('o'); event='test viewer public TCP denied'; seconds=$Seconds } | ConvertTo-Json -Compress | Add-Content "$d\relay-denial-events.jsonl"
    Start-Sleep -Seconds $Seconds
} finally {
    Remove-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
    [pscustomobject]@{ utc=(Get-Date).ToUniversalTime().ToString('o'); event='test viewer public TCP restored' } | ConvertTo-Json -Compress | Add-Content "$d\relay-denial-events.jsonl"
}
