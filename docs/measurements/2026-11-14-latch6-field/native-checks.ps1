param([string]$Prefix='')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$d = "$env:USERPROFILE\dev\latch6-field-final-20261114"
Set-Location $d
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\dev\latch6-field-20261114\target"
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
function CargoCheck($label, $arguments) {
    $label=$Prefix+$label
    $start = Get-Date
    $ErrorActionPreference = 'Continue'
    & $cargo ($arguments -split ' ') 1> "$d\$label.out" 2> "$d\$label.err"
    $result = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    [pscustomobject]@{ label=$label; exit=$result; elapsed_s=((Get-Date)-$start).TotalSeconds; command="cargo $arguments" } | ConvertTo-Json -Compress | Add-Content "$d\${Prefix}native-results.jsonl"
}
CargoCheck 'foundation' 'test --locked -p latch-core -p latch-proto -p latch-relay --all-targets'
CargoCheck 'platform-libs' 'test --locked -p latch-engine -p latch-stream -p latch-client --lib'
CargoCheck 'lifecycle' 'test --locked -p latch-engine --test lifecycle -- --test-threads=1'
CargoCheck 'relay-runtime' 'test --locked -p latch-engine --test relay_runtime -- --test-threads=1'
CargoCheck 'windows-examples' 'build --locked -p latch-engine --example serve --example pipeline_bench -p latch-client --example present -p latch-stream --example view --example pads'
CargoCheck 'workspace-check' 'check --locked --workspace --all-targets'
Set-Content "$d\${Prefix}native-done.txt" (Get-Date -Format o)
Get-Content "$d\${Prefix}native-results.jsonl"
