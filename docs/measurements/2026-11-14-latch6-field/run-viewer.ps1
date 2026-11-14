param([string]$Label='udp-1080-60', [int]$Fps=60, [int]$Seconds=35, [switch]$Automatic, [switch]$Audio, [int]$Width=1920, [int]$Height=1080, [int]$Kbps=20000)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$d = "$env:USERPROFILE\dev\latch6-field-final-20261114"
Set-Location $d
$env:RUST_LOG='latch_stream=info,latch_client=info'
$env:LATCH_FIELD_KBPS=[string]$Kbps
if ($Automatic) { Remove-Item Env:LATCH_FIELD_UDP_ONLY -ErrorAction SilentlyContinue } else { $env:LATCH_FIELD_UDP_ONLY='1' }
if ($Audio) { $env:LATCH_FIELD_AUDIO='1' } else { Remove-Item Env:LATCH_FIELD_AUDIO -ErrorAction SilentlyContinue }
$p = Start-Process "$env:USERPROFILE\dev\latch6-field-20261114\target\debug\latch-field-viewer.exe" -ArgumentList "100.111.100.57 --port 47964 --width $Width --height $Height --fps $Fps --seconds $Seconds --h264" -RedirectStandardOutput "$d\$Label.out" -RedirectStandardError "$d\$Label.err" -PassThru
$null=$p.Handle
try {
    [pscustomobject]@{ label=$Label; pid=$p.Id; session_id=$p.SessionId; utc=(Get-Date).ToUniversalTime().ToString('o'); fps=$Fps; seconds=$Seconds; automatic=[bool]$Automatic; audio=[bool]$Audio } | ConvertTo-Json -Compress | Set-Content "$d\$Label.meta.json"
    $deadline=(Get-Date).AddSeconds($Seconds+25)
    while (-not $p.HasExited -and (Get-Date) -lt $deadline) {
        Get-NetTCPConnection -OwningProcess $p.Id -ErrorAction SilentlyContinue | ForEach-Object {
            [pscustomobject]@{ utc=(Get-Date).ToUniversalTime().ToString('o'); local_ip=$_.LocalAddress; local_port=$_.LocalPort; remote_ip=$_.RemoteAddress; remote_port=$_.RemotePort; state=[string]$_.State } | ConvertTo-Json -Compress | Add-Content "$d\$Label.sockets.jsonl"
        }
        Start-Sleep -Seconds 1
        $p.Refresh()
    }
    if (-not $p.HasExited) { Stop-Process -Id $p.Id; throw 'field viewer exceeded deadline' }
    $p.WaitForExit()
    "exit=$($p.ExitCode)" | Set-Content "$d\$Label.exit"
    Get-Content "$d\$Label.err" -Tail 12
} finally {
    $p.Refresh()
    if (-not $p.HasExited) { Stop-Process -Id $p.Id }
}
