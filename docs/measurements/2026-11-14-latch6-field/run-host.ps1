$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$d="$env:USERPROFILE\dev\latch6-field-final-20261114"
$binary="$env:USERPROFILE\dev\latch6-field-20261114\target\debug\examples\serve.exe"
Set-Location $d
$env:LATCH_TEST_PATTERN='sweep'
$env:LATCH_TEST_AUDIO='clicks'
$env:RUST_LOG='info'
$names=@('Latch6Field20261114TCP','Latch6Field20261114UDP')
$p=$null
try {
    foreach ($protocol in 'TCP','UDP') {
        New-NetFirewallRule -Name "Latch6Field20261114$protocol" -DisplayName "Latch6 field validation $protocol" -Direction Inbound -Action Allow -Protocol $protocol -LocalPort 47968 -RemoteAddress 100.111.100.57 -Program $binary -Profile Any | Out-Null
    }
    $p=Start-Process $binary -ArgumentList '--bind 100.106.23.40 --port 47968 --telemetry' -RedirectStandardOutput "$d\windows-host.out" -RedirectStandardError "$d\windows-host.err" -PassThru
    $null=$p.Handle
    [pscustomobject]@{ pid=$p.Id; session_id=$p.SessionId; bind='100.106.23.40:47968'; pattern='sweep'; utc=(Get-Date).ToUniversalTime().ToString('o') } | ConvertTo-Json -Compress | Set-Content "$d\windows-host.meta.json"
    $deadline=(Get-Date).AddMinutes(10)
    while (-not $p.HasExited -and (Get-Date) -lt $deadline -and -not (Test-Path "$d\stop-host")) {
        Start-Sleep -Seconds 1
        $p.Refresh()
    }
} finally {
    if ($p) { $p.Refresh(); if (-not $p.HasExited) { Stop-Process -Id $p.Id } }
    foreach ($name in $names) { Remove-NetFirewallRule -Name $name -ErrorAction SilentlyContinue }
    Remove-Item "$d\stop-host" -ErrorAction SilentlyContinue
    Set-Content "$d\windows-host-cleanup.txt" (Get-Date -Format o)
}
