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

Push-Location $ProjectRoot
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}

if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Release executable not found: $ExePath"
}

$exe = Get-Item -LiteralPath $ExePath
if ($exe.Length -le 0) {
    throw "Release executable is empty: $ExePath"
}

if (-not ("ProcessLauncher" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;

public sealed class ProcessLaunch : IDisposable {
    private IntPtr processHandle;

    public int ProcessId { get; private set; }
    public int SessionId { get; private set; }

    internal ProcessLaunch(IntPtr processHandle, int processId, int sessionId) {
        this.processHandle = processHandle;
        ProcessId = processId;
        SessionId = sessionId;
    }

    public bool WaitForExit(int milliseconds, out uint exitCode) {
        uint result = ProcessLauncher.WaitForSingleObject(processHandle, (uint)milliseconds);
        if (result == ProcessLauncher.WAIT_TIMEOUT) {
            exitCode = 0;
            return false;
        }
        if (result != ProcessLauncher.WAIT_OBJECT_0) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "WaitForSingleObject failed");
        }
        if (!ProcessLauncher.GetExitCodeProcess(processHandle, out exitCode)) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "GetExitCodeProcess failed");
        }
        return true;
    }

    public void Dispose() {
        if (processHandle != IntPtr.Zero) {
            ProcessLauncher.CloseHandle(processHandle);
            processHandle = IntPtr.Zero;
        }
    }
}

public static class ProcessLauncher {
    internal const uint WAIT_OBJECT_0 = 0;
    internal const uint WAIT_TIMEOUT = 258;
    private const uint INVALID_SESSION_ID = 0xFFFFFFFF;
    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    private const uint TOKEN_ASSIGN_PRIMARY = 0x0001;
    private const uint TOKEN_DUPLICATE = 0x0002;
    private const uint TOKEN_QUERY = 0x0008;
    private const uint LOGON_WITH_PROFILE = 0x00000001;
    private const uint CREATE_UNICODE_ENVIRONMENT = 0x00000400;
    private const int SecurityImpersonation = 2;
    private const int TokenPrimary = 1;

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
    private static extern bool CreateProcess(
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

    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CreateProcessWithTokenW(
        IntPtr hToken,
        uint dwLogonFlags,
        string lpApplicationName,
        string lpCommandLine,
        uint dwCreationFlags,
        IntPtr lpEnvironment,
        string lpCurrentDirectory,
        ref STARTUPINFO lpStartupInfo,
        out PROCESS_INFORMATION lpProcessInformation
    );

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(
        uint dwDesiredAccess,
        bool bInheritHandle,
        uint dwProcessId
    );

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool OpenProcessToken(
        IntPtr processHandle,
        uint desiredAccess,
        out IntPtr tokenHandle
    );

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool DuplicateTokenEx(
        IntPtr existingToken,
        uint desiredAccess,
        IntPtr tokenAttributes,
        int impersonationLevel,
        int tokenType,
        out IntPtr newToken
    );

    [DllImport("userenv.dll", SetLastError = true)]
    private static extern bool CreateEnvironmentBlock(
        out IntPtr environment,
        IntPtr token,
        bool inherit
    );

    [DllImport("userenv.dll", SetLastError = true)]
    private static extern bool DestroyEnvironmentBlock(IntPtr environment);

    [DllImport("kernel32.dll")]
    private static extern uint WTSGetActiveConsoleSessionId();

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool CloseHandle(IntPtr hObject);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

    private static HashSet<int> ExplorerSessionIds() {
        HashSet<int> sessions = new HashSet<int>();
        foreach (Process process in Process.GetProcessesByName("explorer")) {
            using (process) {
                try {
                    sessions.Add(process.SessionId);
                }
                catch (InvalidOperationException) {
                    // Explorer exited while it was being inspected.
                }
            }
        }
        return sessions;
    }

    private static int SelectInteractiveSession() {
        int currentSession = Process.GetCurrentProcess().SessionId;
        HashSet<int> explorerSessions = ExplorerSessionIds();
        if (explorerSessions.Contains(currentSession)) {
            return currentSession;
        }

        uint activeConsoleSession = WTSGetActiveConsoleSessionId();
        if (activeConsoleSession != INVALID_SESSION_ID && explorerSessions.Contains((int)activeConsoleSession)) {
            return (int)activeConsoleSession;
        }

        if (explorerSessions.Count == 1) {
            foreach (int session in explorerSessions) {
                return session;
            }
        }

        if (explorerSessions.Count == 0) {
            throw new InvalidOperationException("No logged-in interactive Explorer session was found.");
        }
        throw new InvalidOperationException(
            "Multiple interactive Explorer sessions were found and no active console session could be selected. Run the script from the intended user session."
        );
    }

    private static uint FindExplorerProcessId(int sessionId) {
        foreach (Process process in Process.GetProcessesByName("explorer")) {
            using (process) {
                try {
                    if (process.SessionId == sessionId) {
                        return (uint)process.Id;
                    }
                }
                catch (InvalidOperationException) {
                    // Explorer exited while it was being inspected.
                }
            }
        }
        throw new InvalidOperationException("Explorer exited before the target session could be opened.");
    }

    private static PROCESS_INFORMATION StartInCurrentSession(
        string exePath,
        string workingDir,
        ref STARTUPINFO startupInfo
    ) {
        PROCESS_INFORMATION processInfo;
        if (!CreateProcess(
            exePath,
            null,
            IntPtr.Zero,
            IntPtr.Zero,
            false,
            0,
            IntPtr.Zero,
            workingDir,
            ref startupInfo,
            out processInfo
        )) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcess failed");
        }
        return processInfo;
    }

    private static PROCESS_INFORMATION StartInAnotherSession(
        string exePath,
        string workingDir,
        int sessionId,
        ref STARTUPINFO startupInfo
    ) {
        IntPtr processHandle = IntPtr.Zero;
        IntPtr sourceToken = IntPtr.Zero;
        IntPtr primaryToken = IntPtr.Zero;
        IntPtr environment = IntPtr.Zero;
        try {
            processHandle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                FindExplorerProcessId(sessionId)
            );
            if (processHandle == IntPtr.Zero) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcess(explorer.exe) failed");
            }
            if (!OpenProcessToken(
                processHandle,
                TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
                out sourceToken
            )) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken(explorer.exe) failed");
            }
            if (!DuplicateTokenEx(
                sourceToken,
                TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
                IntPtr.Zero,
                SecurityImpersonation,
                TokenPrimary,
                out primaryToken
            )) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "DuplicateTokenEx failed");
            }
            if (!CreateEnvironmentBlock(out environment, primaryToken, false)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateEnvironmentBlock failed");
            }

            PROCESS_INFORMATION processInfo;
            if (!CreateProcessWithTokenW(
                primaryToken,
                LOGON_WITH_PROFILE,
                exePath,
                null,
                CREATE_UNICODE_ENVIRONMENT,
                environment,
                workingDir,
                ref startupInfo,
                out processInfo
            )) {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "CreateProcessWithTokenW failed; run the script elevated when launching across sessions"
                );
            }
            return processInfo;
        }
        finally {
            if (environment != IntPtr.Zero) {
                DestroyEnvironmentBlock(environment);
            }
            if (primaryToken != IntPtr.Zero) {
                CloseHandle(primaryToken);
            }
            if (sourceToken != IntPtr.Zero) {
                CloseHandle(sourceToken);
            }
            if (processHandle != IntPtr.Zero) {
                CloseHandle(processHandle);
            }
        }
    }

    public static ProcessLaunch StartOnInteractiveDesktop(string exePath, string workingDir) {
        int targetSession = SelectInteractiveSession();
        int currentSession = Process.GetCurrentProcess().SessionId;
        STARTUPINFO si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(si);
        si.lpDesktop = @"WinSta0\Default";

        PROCESS_INFORMATION pi = targetSession == currentSession
            ? StartInCurrentSession(exePath, workingDir, ref si)
            : StartInAnotherSession(exePath, workingDir, targetSession, ref si);
        try {
            return new ProcessLaunch(pi.hProcess, pi.dwProcessId, targetSession);
        }
        catch {
            CloseHandle(pi.hProcess);
            throw;
        }
        finally {
            CloseHandle(pi.hThread);
        }
    }
}
"@
}

$launch = [ProcessLauncher]::StartOnInteractiveDesktop($ExePath, $ProjectRoot)
try {
    $exitCode = [uint32]0
    if ($launch.WaitForExit(1000, [ref]$exitCode)) {
        throw "$AppName exited immediately with code $exitCode."
    }

    Write-Host "Started release $AppName on session $($launch.SessionId), WinSta0\Default (PID $($launch.ProcessId))."
}
finally {
    $launch.Dispose()
}
