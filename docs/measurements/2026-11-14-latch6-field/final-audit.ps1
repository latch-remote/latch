param([string]$Prefix='field-completed')
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$d="$env:USERPROFILE\dev\latch6-field-final-20261114"
$target="$env:USERPROFILE\dev\latch6-field-20261114\target"
$manifest=Get-Content "$d\audit-manifest.json" -Raw | ConvertFrom-Json
$mismatches=@(foreach ($row in $manifest) {
    $path=Join-Path $d $row.path
    if (-not (Test-Path $path)) { $row.path }
    elseif ((Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $row.sha256) { $row.path }
})
$binaries=@(foreach ($name in @('debug\latch-field-viewer.exe','debug\examples\serve.exe','debug\examples\view.exe')) {
    $path=Join-Path $target $name
    if (Test-Path $path) {
        [pscustomobject]@{path=$path;bytes=(Get-Item $path).Length;sha256=(Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()}
    }
})
$task=Get-ScheduledTask -TaskName 'Latch6Field20261114' -ErrorAction SilentlyContinue
if ($task -and $task.State -ne 'Running') { Unregister-ScheduledTask -TaskName 'Latch6Field20261114' -Confirm:$false }
$ruleNames=@('Latch6Field20261114TCP','Latch6Field20261114UDP','Latch6FieldPublicRelayDeny20261114','Latch6FieldReverseUDPInbound','Latch6FieldReverseUDPOutbound')
$rules=@(foreach ($name in $ruleNames) { Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue | Select-Object Name,Enabled })
$processes=@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like "$env:USERPROFILE\dev\latch6-field*" } | Select-Object ProcessId,Name,ExecutablePath)
$result=[pscustomobject]@{
    utc=(Get-Date).ToUniversalTime().ToString('o');checked=$manifest.Count;mismatches=$mismatches
    binaries=$binaries;owned_rules=$rules;owned_processes=$processes
    owned_task=@(Get-ScheduledTask -TaskName 'Latch6Field20261114' -ErrorAction SilentlyContinue | Select-Object TaskName,State)
    os=[Environment]::OSVersion.VersionString
    rust=(& "$env:USERPROFILE\.cargo\bin\rustc.exe" --version)
}
$result | ConvertTo-Json -Depth 6 | Set-Content "$d\$Prefix-audit.json"
$result | ConvertTo-Json -Depth 6
if ($mismatches.Count -gt 0 -or $rules.Count -gt 0 -or $processes.Count -gt 0) { exit 1 }
