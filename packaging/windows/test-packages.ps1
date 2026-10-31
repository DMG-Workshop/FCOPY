param([string]$PackageDirectory = (Join-Path $PSScriptRoot '..\..\dist'))

$ErrorActionPreference = 'Stop'
$directory = (Resolve-Path -LiteralPath $PackageDirectory).Path
$installers = @(Get-ChildItem -LiteralPath $directory -Filter 'FCOPY-*-windows-x86_64-unsigned-setup.exe')
$archives = @(Get-ChildItem -LiteralPath $directory -Filter 'FCOPY-*-windows-x86_64-unsigned-portable.zip')
if ($installers.Count -ne 1 -or $archives.Count -ne 1) {
    throw 'Expected exactly one unsigned Windows installer and portable ZIP'
}
$installer = $installers[0].FullName
$archive = $archives[0].FullName
if ($installers[0].Name.Replace('-setup.exe', '') -ne $archives[0].Name.Replace('-portable.zip', '')) {
    throw 'Installer and portable ZIP versions differ'
}
$verbs = @(
    'Registry::HKEY_CURRENT_USER\Software\Classes\*\shell\fcopy',
    'Registry::HKEY_CURRENT_USER\Software\Classes\Directory\shell\fcopy'
)
$registration = 'Registry::HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall\{8EEC91B4-CA64-4EBC-BCFB-B406FC694692}_is1'
foreach ($key in @($registration) + $verbs) {
    if (Test-Path -LiteralPath $key) {
        throw "Refusing to overwrite an existing FCOPY installation/integration: $key. Use a disposable user."
    }
}
$startMenu = Join-Path ([Environment]::GetFolderPath('Programs')) 'FCOPY\FCOPY.lnk'
$desktop = Join-Path ([Environment]::GetFolderPath('Desktop')) 'FCOPY.lnk'
foreach ($shortcut in @($startMenu, $desktop)) {
    if (Test-Path -LiteralPath $shortcut) { throw "Refusing to overwrite existing shortcut: $shortcut" }
}
$work = Join-Path ([IO.Path]::GetTempPath()) ('fcopy-package-test-' + [guid]::NewGuid().ToString('N'))
$install = Join-Path $work 'Installed FCOPY'
$portable = Join-Path $work 'Portable FCOPY'
$previousConfig = $env:FCOPY_CONFIG_DIR
$succeeded = $false

function Invoke-Checked {
    param([string]$Executable, [string[]]$Arguments = @())
    $stdout = Join-Path $work 'process.stdout.log'
    $stderr = Join-Path $work 'process.stderr.log'
    $options = @{
        FilePath = $Executable
        PassThru = $true
        RedirectStandardOutput = $stdout
        RedirectStandardError = $stderr
    }
    if ($Arguments.Count) { $options.ArgumentList = ($Arguments | ForEach-Object { '"' + $_ + '"' }) -join ' ' }
    $child = Start-Process @options
    try {
        if (-not $child.WaitForExit(120000)) { throw "$Executable timed out" }
        if ($child.ExitCode -ne 0) {
            Get-Content -LiteralPath $stdout, $stderr -ErrorAction Continue | Write-Host
            throw "$Executable exited with code $($child.ExitCode)"
        }
    } finally {
        if (-not $child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
        $child.Dispose()
    }
}

function Assert-NoVerbs {
    foreach ($key in $verbs) {
        if (Test-Path -LiteralPath $key) { throw "Unexpected Explorer registration: $key" }
    }
}

function Assert-Verbs {
    param([string]$Gui)
    foreach ($key in $verbs) {
        $command = (Get-Item -LiteralPath "$key\command").GetValue('')
        if ($command -ne "`"$Gui`" --sources %*") { throw "Unexpected Explorer command: $command" }
        $model = (Get-Item -LiteralPath $key).GetValue('MultiSelectModel')
        if ($model -ne 'Player') { throw 'Explorer multi-select contract is missing' }
    }
}

function Assert-Copy {
    param([string]$Cli, [string]$Destination)
    Invoke-Checked $Cli @('copy', (Join-Path $work 'source file.bin'), $Destination, '--verify', 'readback', '--on-error', 'cancel')
    if ((Get-FileHash -LiteralPath $Destination).Hash -ne $script:sourceHash) {
        throw 'Packaged CLI read-back copy differs from its source'
    }
}

function Invoke-Install {
    param([string]$Tasks = '')
    Invoke-Checked $installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
        "/DIR=$install", "/TASKS=$Tasks", "/LOG=$(Join-Path $work 'setup.log')")
}

function Invoke-Uninstall {
    $log = Join-Path $work 'uninstall.log'
    Invoke-Checked (Join-Path $install 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART',
        "/LOG=$log")
    # Inno's self-removal helper can outlive the original uninstaller process.
    $deadline = (Get-Date).AddSeconds(15)
    while ($true) {
        try {
            $handle = [IO.File]::Open($log, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
            $handle.Dispose()
            break
        } catch [IO.IOException] {
            $code = $_.Exception.HResult -band 0xffff
            if ($code -notin @(32, 33) -or (Get-Date) -ge $deadline) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
    if (Test-Path -LiteralPath (Join-Path $install 'fcopy.exe')) { throw 'Uninstall left the installed CLI' }
    if (Test-Path -LiteralPath (Join-Path $install 'fcopy-gui.exe')) { throw 'Uninstall left the installed GUI' }
    if (Test-Path -LiteralPath $registration) { throw 'Uninstall left Installed apps registration' }
    foreach ($shortcut in @($startMenu, $desktop)) {
        if (Test-Path -LiteralPath $shortcut) { throw "Uninstall left shortcut: $shortcut" }
    }
}

New-Item -ItemType Directory -Path $work | Out-Null
try {
    $env:FCOPY_CONFIG_DIR = Join-Path $work 'User state'
    New-Item -ItemType Directory -Path $env:FCOPY_CONFIG_DIR | Out-Null
    $sentinel = Join-Path $env:FCOPY_CONFIG_DIR 'preserve-user-data.txt'
    [IO.File]::WriteAllText($sentinel, 'settings profiles history recovery must remain')
    $bytes = [byte[]]::new(65539)
    [Random]::new(42).NextBytes($bytes)
    $source = Join-Path $work 'source file.bin'
    [IO.File]::WriteAllBytes($source, $bytes)
    $script:sourceHash = (Get-FileHash -LiteralPath $source).Hash
    $sentinelHash = (Get-FileHash -LiteralPath $sentinel).Hash
    if ((Get-AuthenticodeSignature -LiteralPath $installer).Status -ne 'NotSigned') {
        throw 'Installer is not unsigned as labeled'
    }
    Expand-Archive -LiteralPath $archive -DestinationPath $portable
    $payload = @(Get-ChildItem -LiteralPath $portable -Recurse -File)
    if ($payload.Count -ne 4) { throw 'Portable payload must have only CLI, GUI, README and unsigned notice' }
    foreach ($name in @('fcopy.exe', 'fcopy-gui.exe')) {
        if ((Get-AuthenticodeSignature -LiteralPath (Join-Path $portable $name)).Status -ne 'NotSigned') {
            throw "$name is not unsigned as labeled"
        }
    }
    if (-not (Select-String -LiteralPath (Join-Path $portable 'UNSIGNED.txt') -SimpleMatch 'UNSIGNED WINDOWS BUILD')) {
        throw 'Portable unsigned notice is missing'
    }
    if ($archives[0].Name -notmatch '^FCOPY-(\d+\.\d+\.\d+)-windows-') { throw 'Invalid package version' }
    $version = $Matches[1]
    $portableGui = Join-Path $portable 'fcopy-gui.exe'
    if ([Diagnostics.FileVersionInfo]::GetVersionInfo($portableGui).FileVersion -ne $version) {
        throw 'Portable GUI resource version differs from the download filename'
    }
    Assert-Copy (Join-Path $portable 'fcopy.exe') (Join-Path $work 'portable copy.bin')
    Assert-NoVerbs

    Invoke-Install
    $installedGui = Join-Path $install 'fcopy-gui.exe'
    foreach ($name in @('fcopy.exe', 'fcopy-gui.exe', 'README.txt', 'UNSIGNED.txt')) {
        if ((Get-FileHash -LiteralPath (Join-Path $portable $name)).Hash -ne
            (Get-FileHash -LiteralPath (Join-Path $install $name)).Hash) { throw "Installed $name differs from portable payload" }
    }
    if (-not (Test-Path -LiteralPath $registration)) { throw 'Per-user Installed apps entry is missing' }
    if (-not (Test-Path -LiteralPath $startMenu)) { throw 'Start menu shortcut is missing' }
    if (Test-Path -LiteralPath $desktop) { throw 'Desktop shortcut was not opted in' }
    Assert-NoVerbs
    Assert-Copy (Join-Path $install 'fcopy.exe') (Join-Path $work 'installed copy.bin')
    Invoke-Uninstall
    Assert-NoVerbs

    Invoke-Install 'explorer,desktopicon'
    Assert-Verbs $installedGui
    if (-not (Test-Path -LiteralPath $desktop)) { throw 'Opt-in desktop shortcut is missing' }
    # Upgrading with the task still enabled keeps integration on the installed path.
    Invoke-Install 'explorer,desktopicon'
    Assert-Verbs $installedGui
    Invoke-Uninstall
    Assert-NoVerbs

    Invoke-Install 'explorer'
    Assert-Verbs $installedGui
    # Explicitly deselecting the task on upgrade removes only owned integration.
    Invoke-Install
    Assert-NoVerbs
    Invoke-Install 'explorer'
    Assert-Verbs $installedGui
    Invoke-Checked $portableGui @('--install-integration')
    Assert-Verbs $portableGui
    Invoke-Uninstall
    Assert-Verbs $portableGui
    Invoke-Checked $portableGui @('--uninstall-integration')
    Assert-NoVerbs

    if ((Get-FileHash -LiteralPath $sentinel).Hash -ne $sentinelHash) { throw 'Uninstall modified user configuration' }
    foreach ($copy in @('portable copy.bin', 'installed copy.bin', 'source file.bin')) {
        if ((Get-FileHash -LiteralPath (Join-Path $work $copy)).Hash -ne $script:sourceHash) {
            throw "Uninstall modified transfer data: $copy"
        }
    }
    $succeeded = $true
} finally {
    if (-not $succeeded) {
        foreach ($log in @('setup.log', 'uninstall.log')) {
            $path = Join-Path $work $log
            if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path -Tail 35 | Write-Host }
        }
        if (Test-Path -LiteralPath (Join-Path $install 'unins000.exe')) { Invoke-Uninstall }
        if (Test-Path -LiteralPath (Join-Path $portable 'fcopy-gui.exe')) {
            Invoke-Checked (Join-Path $portable 'fcopy-gui.exe') @('--uninstall-integration')
        }
    }
    $env:FCOPY_CONFIG_DIR = $previousConfig
    Remove-Item -LiteralPath $work -Recurse -Force
}
Write-Output 'PASS: unsigned packages, portable/installed read-back copies, opt-in integration, upgrades, ownership-aware removal and preserved data'
