param(
    [switch]$StopOnly
)

$ErrorActionPreference = "Stop"

$ProjectRoot = $PSScriptRoot
$AppName = "tacky-borders"
$ExePath = Join-Path $ProjectRoot "target\release\tacky-borders.exe"

function Stop-TackyBorders {
    $processes = @(Get-Process -Name $AppName -ErrorAction SilentlyContinue)
    if ($processes.Count -eq 0) {
        return
    }

    Write-Host "Stopping existing $AppName process(es)..."
    foreach ($process in $processes) {
        try {
            Stop-Process -Id $process.Id -Force -ErrorAction Stop
            if (-not $process.WaitForExit(5000)) {
                throw "$AppName (PID $($process.Id)) did not exit within 5 seconds."
            }
        }
        catch {
            throw "Could not stop $AppName (PID $($process.Id)). If it is elevated, run this script from an elevated PowerShell. $($_.Exception.Message)"
        }
    }
}

Stop-TackyBorders

if ($StopOnly) {
    exit 0
}

if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Release executable not found: $ExePath. Build it first with 'cargo build --release'."
}

$exe = Get-Item -LiteralPath $ExePath
if ($exe.Length -le 0) {
    throw "Release executable is empty: $ExePath"
}

$process = Start-Process -FilePath $ExePath -WorkingDirectory $ProjectRoot -PassThru
Start-Sleep -Seconds 1
if ($process.HasExited) {
    throw "$AppName exited immediately with code $($process.ExitCode)."
}

Write-Host "Started release $AppName (PID $($process.Id))."
