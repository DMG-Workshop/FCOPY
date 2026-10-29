# End-to-end check of the fcopy-agent Windows service: install it, copy a file
# to it over QUIC, check the bytes, then clean up. Run from the repo root in an
# elevated PowerShell after `cargo build -p fc-agent -p fc-cli`.
$ErrorActionPreference = 'Stop'
$dir   = 'C:\ProgramData\fcopy-agent'
$agent = '.\target\debug\fcopy-agent.exe'
$cli   = '.\target\debug\fcopy.exe'
$pass  = 'service-pass-1'
$work  = Join-Path $env:TEMP 'fcopy-service-test'

New-Item -ItemType Directory -Force "$dir\share" | Out-Null
New-Item -ItemType Directory -Force $work | Out-Null
$src = Join-Path $work 'svc-test.txt'
Set-Content -Path $src -Value 'hello from the service test' -NoNewline

# Config is read when the service starts, so set it up before starting.
$env:FCOPY_PASSWORD = $pass
& $agent --dir $dir share add backup "$dir\share"
& $agent --dir $dir user add alice --shares backup
Remove-Item Env:FCOPY_PASSWORD
& $agent --dir $dir service install
Start-Service fcopy-agent
try {
    $fp = (& $agent --dir $dir fingerprint).Trim()
    Write-Host "agent fingerprint: $fp"

    $pwFile = Join-Path $work 'pw.txt'
    Set-Content -Path $pwFile -Value $pass
    $env:FCOPY_CONFIG_DIR = Join-Path $work 'client'
    & $cli copy $src 'alice@127.0.0.1:47800:/backup/svc' --accept-fingerprint $fp --password-file $pwFile
    if ($LASTEXITCODE -ne 0) { throw "fcopy copy failed with exit code $LASTEXITCODE" }

    $landed = Join-Path $dir 'share\svc\svc-test.txt'
    $got = Get-Content -Raw $landed
    if ($got -ne 'hello from the service test') { throw "contents differ: '$got'" }
    Write-Host 'PASS: file copied to the Windows service and verified'
}
finally {
    Stop-Service fcopy-agent
    & $agent --dir $dir service uninstall
    & $agent --dir $dir user del alice
    Remove-Item -Recurse -Force $work
    Remove-Item Env:FCOPY_CONFIG_DIR -ErrorAction SilentlyContinue
}
