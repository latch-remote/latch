$ProgressPreference='SilentlyContinue'
$d="$env:USERPROFILE\dev\latch6-field-final-20261114"
Set-Location $d
$env:RUST_LOG='latch_stream=info'
$ErrorActionPreference='Continue'
& "$env:USERPROFILE\dev\latch6-field-20261114\target\debug\examples\view.exe" 100.111.100.57 --port 47964 --width 1920 --height 1080 --fps 60 --kbps 20000 --seconds 10 --keys 10 --audio --mute 1>controller-input.json 2>controller-input.err
"exit=$LASTEXITCODE" | Set-Content controller-input.exit
Get-Content controller-input.err -Tail 6
