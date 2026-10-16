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

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ProcessLauncher {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO {
        public int cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public int dwX;
        public int dwY;
        public int dwXSize;
        public int dwYSize;
        public int dwXCountChars;
        public int dwYCountChars;
        public int dwFillAttribute;
        public int dwFlags;
        public short wShowWindow;
        public short cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput;
        public IntPtr hStdOutput;
        public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION {
        public IntPtr hProcess;
        public IntPtr hThread;
        public int dwProcessId;
        public int dwThreadId;
    }

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern bool CreateProcess(
        string lpApplicationName,
        string lpCommandLine,
        IntPtr lpProcessAttributes,
        IntPtr lpThreadAttributes,
        bool bInheritHandles,
        uint dwCreationFlags,
        IntPtr lpEnvironment,
        string lpCurrentDirectory,
        ref STARTUPINFO lpStartupInfo,
        out PROCESS_INFORMATION lpProcessInformation
    );
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr hObject);

    public static int StartOnDefaultDesktop(string exePath, string workingDir) {
        STARTUPINFO si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(si);
        si.lpDesktop = @"WinSta0\Default";
        PROCESS_INFORMATION pi = new PROCESS_INFORMATION();
        bool ok = CreateProcess(exePath, null, IntPtr.Zero, IntPtr.Zero, false, 0, IntPtr.Zero, workingDir, ref si, out pi);
        if (!ok) {
            int err = Marshal.GetLastWin32Error();
            throw new Exception("CreateProcess failed with error: " + err);
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        return pi.dwProcessId;
    }
}
"@ -ErrorAction SilentlyContinue

$procId = [ProcessLauncher]::StartOnDefaultDesktop($ExePath, $ProjectRoot)
Start-Sleep -Seconds 1
$process = Get-Process -Id $procId -ErrorAction SilentlyContinue
if (-not $process -or $process.HasExited) {
    throw "$AppName exited immediately."
}

Write-Host "Started release $AppName on WinSta0\Default (PID $procId)."
