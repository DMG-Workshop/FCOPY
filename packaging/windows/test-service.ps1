# End-to-end check of the fcopy-agent Windows service: install it, copy a file
# to it over QUIC, check the bytes, then clean up. Run from the repo root in an
# elevated PowerShell after `cargo build -p fc-agent -p fc-cli`.
$ErrorActionPreference = 'Stop'

$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Not elevated. Open PowerShell with "Run as administrator" and run this again.'
}

$dir   = 'C:\ProgramData\fcopy-agent'
$agent = '.\target\debug\fcopy-agent.exe'
$cli   = '.\target\debug\fcopy.exe'
$pass  = 'service-pass-1'
$work  = Join-Path $env:TEMP 'fcopy-service-test'

# Native programs don't stop the script on error, so check the exit code.
function Invoke-Native {
    param([string]$Exe, [string[]]$ArgList)
    & $Exe @ArgList
    if ($LASTEXITCODE -ne 0) { throw "$Exe $($ArgList -join " ") failed with exit code $LASTEXITCODE" }
}

New-Item -ItemType Directory -Force "$dir\share" | Out-Null
New-Item -ItemType Directory -Force $work | Out-Null
$src = Join-Path $work 'svc-test.txt'
Set-Content -Path $src -Value 'hello from the service test' -NoNewline

try {
    # Config is read when the service starts, so set it up before starting.
    $env:FCOPY_PASSWORD = $pass
    Invoke-Native $agent @('--dir', $dir, 'share', 'add', 'backup', "$dir\share")
    Invoke-Native $agent @('--dir', $dir, 'user', 'add', 'alice', '--shares', 'backup')
    Remove-Item Env:FCOPY_PASSWORD
    Invoke-Native $agent @('--dir', $dir, 'service', 'install')
    Start-Service fcopy-agent

    $fp = (& $agent --dir $dir fingerprint).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $fp.StartsWith('SHA256:')) { throw "could not read the agent fingerprint" }
    Write-Host "agent fingerprint: $fp"

    $pwFile = Join-Path $work 'pw.txt'
    Set-Content -Path $pwFile -Value $pass
    $env:FCOPY_CONFIG_DIR = Join-Path $work 'client'
    Invoke-Native $cli @('copy', $src, 'alice@127.0.0.1:47800:/backup/svc', '--accept-fingerprint', $fp, '--password-file', $pwFile)

    $landed = Join-Path $dir 'share\svc\svc-test.txt'
    $got = Get-Content -Raw $landed
    if ($got -ne 'hello from the service test') { throw "contents differ: '$got'" }
    Write-Host 'PASS: file copied to the Windows service and verified'
}
finally {
    # Clean up whatever was done, ignoring steps that never ran.
    Stop-Service fcopy-agent -ErrorAction SilentlyContinue
    & $agent --dir $dir service uninstall 2>$null
    & $agent --dir $dir user del alice 2>$null
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    Remove-Item Env:FCOPY_CONFIG_DIR -ErrorAction SilentlyContinue
}
