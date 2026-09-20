param(
    [switch]$StopOnly
)

$ErrorActionPreference = "Stop"

$ProjectRoot = $PSScriptRoot
$AppName = "tacky-borders"
$ExePath = Join-Path $ProjectRoot "target\debug\tacky-borders.exe"

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

Push-Location $ProjectRoot
try {
    & cargo build
    if ($LASTEXITCODE -ne 0) {
        throw "Debug build failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}

if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Debug executable not found: $ExePath"
}

$exe = Get-Item -LiteralPath $ExePath
if ($exe.Length -le 0) {
    throw "Debug executable is empty: $ExePath"
}

$process = Start-Process -FilePath $ExePath -WorkingDirectory $ProjectRoot -PassThru
Start-Sleep -Seconds 1
if ($process.HasExited) {
    throw "$AppName exited immediately with code $($process.ExitCode)."
}

Write-Host "Started debug $AppName (PID $($process.Id))."
