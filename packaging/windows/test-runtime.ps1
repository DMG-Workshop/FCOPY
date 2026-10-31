param([Parameter(Mandatory)][string[]]$Binary)

$ErrorActionPreference = 'Stop'
$command = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
$dumpbin = if ($command) { $command.Source } else { $null }
if (-not $dumpbin) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $vswhere) {
        $dumpbin = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -Last 1
        if ($LASTEXITCODE -ne 0) { throw 'Cannot locate MSVC tools with vswhere' }
    }
}
if (-not $dumpbin) { throw 'MSVC dumpbin.exe is required to validate Windows runtime dependencies' }

foreach ($path in $Binary) {
    $path = (Resolve-Path -LiteralPath $path).Path
    $output = & $dumpbin /NOLOGO /DEPENDENTS $path
    if ($LASTEXITCODE -ne 0) { throw "Cannot inspect DLL imports: $path" }
    $imports = @($output | ForEach-Object {
        if ($_ -match '^\s+(\S+\.dll)\s*$') { $Matches[1] }
    })
    if ($imports.Count -eq 0) { throw "No DLL imports found; cannot validate $path" }
    $runtime = @($imports | Where-Object {
        $_ -match '^(vcruntime|msvcp|msvcr|concrt|vccorlib).*\.dll$|^ucrtbased\.dll$'
    })
    if ($runtime.Count) {
        throw "$path requires Microsoft Visual C++ runtime DLLs: $($runtime -join ', '). Rebuild with the repository's static MSVC runtime configuration."
    }
    Write-Output "PASS: no redistributable Visual C++ runtime DLLs imported by $path"
}
