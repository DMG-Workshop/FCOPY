# Build the desktop/CLI binaries first with cargo build --locked --release -p fc-cli -p fc-gui.
param(
    [string]$Compiler,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root 'dist' }
if (-not $Compiler) {
    $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($command) { $Compiler = $command.Source }
    foreach ($candidate in @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )) {
        if (-not $Compiler -and (Test-Path -LiteralPath $candidate)) { $Compiler = $candidate }
    }
}
if (-not $Compiler) {
    throw 'Inno Setup 6 is required. Install JRSoftware.InnoSetup or pass -Compiler <ISCC.exe>.'
}
$Compiler = (Resolve-Path -LiteralPath $Compiler).Path
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)

Push-Location -LiteralPath $root
$stage = Join-Path $root ("target\windows-package-" + [guid]::NewGuid().ToString('N'))
try {
    $metadata = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read Cargo package metadata' }
    $version = (($metadata | ConvertFrom-Json).packages | Where-Object name -eq 'fc-gui').version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "Unsupported release version: $version" }
    if ($env:GITHUB_REF_TYPE -eq 'tag' -and $env:GITHUB_REF_NAME -ne "v$version") {
        throw "Tag $env:GITHUB_REF_NAME does not match v$version"
    }
    & (Join-Path $PSScriptRoot 'test-runtime.ps1') -Binary @(
        (Join-Path $root 'target\release\fcopy.exe'),
        (Join-Path $root 'target\release\fcopy-gui.exe')
    )
    New-Item -ItemType Directory -Path $stage, $OutputDirectory -Force | Out-Null
    foreach ($name in @('fcopy.exe', 'fcopy-gui.exe')) {
        $binary = Join-Path $root "target\release\$name"
        $stream = [IO.File]::OpenRead($binary)
        $reader = [IO.BinaryReader]::new($stream)
        try {
            if ($reader.ReadUInt16() -ne 0x5a4d) { throw "$name is not a Windows executable" }
            $stream.Position = 0x3c
            $offset = $reader.ReadInt32()
            $stream.Position = $offset
            if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
                throw "$name must be a native x86-64 PE executable"
            }
        } finally { $reader.Dispose() }
        if ((Get-AuthenticodeSignature -LiteralPath $binary).Status -ne 'NotSigned') {
            throw "$name is not unsigned; update the signing/release contract before distributing signed builds"
        }
        if ($name -eq 'fcopy-gui.exe' -and [Diagnostics.FileVersionInfo]::GetVersionInfo($binary).FileVersion -ne $version) {
            throw 'The GUI Windows version resource does not match Cargo; rebuild after updating fcopy.rc'
        }
        Copy-Item -LiteralPath $binary -Destination $stage
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'README.md') -Destination (Join-Path $stage 'README.txt')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'UNSIGNED.txt') -Destination $stage
    $archive = Join-Path $OutputDirectory "FCOPY-$version-windows-x86_64-unsigned-portable.zip"
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive -Force
    & $Compiler "/DAppVersion=$version" "/DPayloadDir=$stage" "/DOutputDir=$OutputDirectory" `
        (Join-Path $PSScriptRoot 'fcopy.iss')
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed: $LASTEXITCODE" }
    Write-Output "Built unsigned installer and portable ZIP in $OutputDirectory"
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    Pop-Location
}
