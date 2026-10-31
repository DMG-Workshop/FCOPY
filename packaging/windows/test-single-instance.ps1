# Run in an interactive Windows desktop after `cargo build -p fc-gui`.
param([string]$Exe = '.\target\debug\fcopy-gui.exe')

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path -LiteralPath $Exe).Path
$tag = [guid]::NewGuid().ToString('N')
$work = Join-Path ([IO.Path]::GetTempPath()) "fcopy-instance-test-$tag"
$previousConfig = $env:FCOPY_CONFIG_DIR
$primary = $null

if (-not ('FcopyInstanceTest' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class FcopyInstanceTest {
    private delegate bool WindowCallback(IntPtr window, IntPtr data);
    [DllImport("user32.dll")] private static extern bool EnumWindows(WindowCallback callback, IntPtr data);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr window, StringBuilder text, int length);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int command);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr window);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    public static IntPtr FindWindow(uint pid) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((window, data) => {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            if (owner == pid) {
                var text = new StringBuilder(256);
                GetWindowText(window, text, text.Capacity);
                if (text.ToString() == "fcopy") { found = window; return false; }
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
'@
}

function Wait-Until {
    param([scriptblock]$Condition, [string]$Failure)
    $deadline = (Get-Date).AddSeconds(15)
    while (-not (& $Condition)) {
        if ((Get-Date) -ge $deadline) { throw $Failure }
        Start-Sleep -Milliseconds 100
    }
}

function Invoke-Launch {
    param([string[]]$Paths = @())
    $options = @{ FilePath = $exePath; PassThru = $true }
    if ($Paths.Count) {
        $options.ArgumentList = ($Paths | ForEach-Object { '"' + $_ + '"' }) -join ' '
    }
    $child = Start-Process @options
    try {
        if (-not $child.WaitForExit(10000)) { throw 'Secondary launch opened another window or stalled' }
        if ($child.ExitCode -ne 0) { throw "Secondary launch exited with code $($child.ExitCode)" }
    } finally {
        if (-not $child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
        $child.Dispose()
    }
}

New-Item -ItemType Directory -Path $work | Out-Null
try {
    $env:FCOPY_CONFIG_DIR = Join-Path $work 'state'
    $source = Join-Path $work 'selected file.txt'
    $secondSource = Join-Path $work 'other file.txt'
    $destination = Join-Path $work 'copied.txt'
    $deferredDestination = Join-Path $work 'deferred.txt'
    [IO.File]::WriteAllText($source, 'first selection')
    [IO.File]::WriteAllText($secondSource, 'second selection')
    $primary = Start-Process -FilePath $exePath -PassThru `
        -RedirectStandardOutput (Join-Path $work 'stdout.log') `
        -RedirectStandardError (Join-Path $work 'stderr.log')
    Wait-Until {
        $primary.Refresh()
        if ($primary.HasExited) { throw 'Primary exited before opening its window' }
        [FcopyInstanceTest]::FindWindow($primary.Id) -ne [IntPtr]::Zero
    } 'Primary window did not appear'
    $window = [FcopyInstanceTest]::FindWindow($primary.Id)

    Invoke-Launch -Paths @($source, $destination)
    Wait-Until { Test-Path -LiteralPath $destination } 'Forwarded copy did not run in the primary queue'
    if ([IO.File]::ReadAllText($destination) -ne 'first selection') { throw 'Copied bytes differ' }

    [FcopyInstanceTest]::ShowWindow($window, 6) | Out-Null
    Wait-Until { [FcopyInstanceTest]::IsIconic($window) } 'Could not minimize the primary'
    Invoke-Launch
    Wait-Until { -not [FcopyInstanceTest]::IsIconic($window) } 'Repeated launch did not restore the primary'
    Wait-Until { [FcopyInstanceTest]::GetForegroundWindow() -eq $window } 'Repeated launch did not focus the primary'

    Invoke-Launch -Paths @('--sources', $secondSource)
    Start-Sleep -Milliseconds 500
    Invoke-Launch -Paths @($source, $deferredDestination)
    Start-Sleep -Milliseconds 500
    if (Test-Path -LiteralPath $deferredDestination) { throw 'Incoming launch replaced an open Add task draft' }
    $primary.Refresh()
    if ($primary.HasExited -or [FcopyInstanceTest]::FindWindow($primary.Id) -ne $window) { throw 'Primary window changed' }
    Write-Host 'PASS: forwarded copy is byte-identical; secondary launches exit; existing window restores and focuses; open draft is preserved'
} catch {
    $diagnostics = Join-Path $work 'stderr.log'
    if (Test-Path -LiteralPath $diagnostics) { Get-Content -LiteralPath $diagnostics -Tail 20 | Write-Host }
    throw
} finally {
    if ($null -ne $primary) {
        $primary.Refresh()
        if (-not $primary.HasExited) {
            $window = [FcopyInstanceTest]::FindWindow($primary.Id)
            if ($window -ne [IntPtr]::Zero) {
                [FcopyInstanceTest]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            }
            if (-not $primary.WaitForExit(10000)) { Stop-Process -Id $primary.Id; $primary.WaitForExit() }
        }
        $primary.Dispose()
    }
    $env:FCOPY_CONFIG_DIR = $previousConfig
    $resolved = [IO.Path]::GetFullPath($work)
    if ((Split-Path -Leaf $resolved) -eq "fcopy-instance-test-$tag") {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    } else {
        throw "Unexpected test cleanup path: $resolved"
    }
}
