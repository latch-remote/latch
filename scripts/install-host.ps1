# Install Latch Host for the current user: copy the exe into
# %LOCALAPPDATA%\Latch, add Start Menu and Desktop shortcuts and an entry
# in Settings > Apps, and open it. The app itself runs the one
# administrator step (firewall, Wake-on-LAN, virtual-display modes) when you
# click "Set up as administrator" on its Sharing tab. Uninstalling (from
# Settings > Apps, or `latch-host.exe --uninstall`) undoes both.
#
# -NoStart leaves the window closed. The background service this stops to
# replace the exe always comes back, in the signed-in user's session: run
# over SSH (session 0, no desktop) it is started there through a one-off
# scheduled task, since a copy started in the SSH session could not see the
# desktop and ends with it.
[CmdletBinding()]
param(
    [string]$Exe = "",
    [switch]$NoStart
)

$ErrorActionPreference = "Stop"
if (-not $Exe) {
    # Beside this script in the release zip; in the repo, under target\release.
    $Here = Split-Path -Parent $MyInvocation.MyCommand.Path
    $Root = Split-Path -Parent $Here
    foreach ($c in @(
            (Join-Path $Here "latch-host.exe"),
            (Join-Path $Root "latch-host.exe"),
            (Join-Path $Root "target\release\latch-host.exe")
        )) {
        if (Test-Path $c) { $Exe = $c; break }
    }
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    Write-Error "latch-host.exe not found. Download a release or build with: cargo build --release -p latch-host"
}

$DestDir = Join-Path $env:LOCALAPPDATA "Latch"
# Where versions before 4.1 installed. Their
# settings come across, and the folder
# goes once this one is in place.
$OldDir = Join-Path $env:LOCALAPPDATA "BroLink"
$DestExe = Join-Path $DestDir "latch-host.exe"

# A running service or panel holds the old exe open; stop them first.
try {
    $ControlHeaders = @{}
    $ControlToken = Join-Path $DestDir 'local-control-token'
    if (Test-Path -LiteralPath $ControlToken) {
        $ControlHeaders['x-latch-local-control'] = (Get-Content -LiteralPath $ControlToken -Raw).Trim()
    }
    Invoke-RestMethod -Headers $ControlHeaders -Method Post -Uri "http://127.0.0.1:47850/v1/quit" -ContentType 'application/json' -Body '{}' -TimeoutSec 2 | Out-Null
    Start-Sleep -Milliseconds 800
} catch {}
# Only this user's host at the path being replaced: the secure-desktop
# helper also runs as latch-host.exe, as SYSTEM from Program Files, and
# stopping it unelevated fails with access denied. Each candidate is opened
# once, and its path, user and start time are checked through that very
# handle before it is ended through it: a process id reused since the list
# was taken is never stopped, and the path is read the same way from 32-bit
# PowerShell as from 64-bit (where Get-Process gives none for a 64-bit
# process).
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;
public static class LatchInstallStop {
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder name, ref uint size);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int infoClass, IntPtr info, int length, out int needed);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out long created, out long exited, out long kernel, out long user);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")]
    static extern uint WaitForSingleObject(IntPtr handle, uint millis);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);
    const uint Terminate = 0x0001, QueryLimited = 0x1000, Synchronize = 0x00100000, TokenQuery = 0x0008;

    // Ends process `pid` if it is `path`, run by `sid`, created at `created`
    // (a FILETIME, within a millisecond). False when it is not, or cannot be
    // opened (another user's, such as the SYSTEM helper).
    public static bool Stop(uint pid, string path, string sid, long created) {
        IntPtr process = OpenProcess(Terminate | QueryLimited | Synchronize, false, pid);
        if (process == IntPtr.Zero) { return false; }
        try {
            var name = new StringBuilder(32768);
            uint size = (uint)name.Capacity;
            if (!QueryFullProcessImageNameW(process, 0, name, ref size)) { return false; }
            if (!string.Equals(name.ToString(), path, StringComparison.OrdinalIgnoreCase)) { return false; }
            long start, exited, kernel, user;
            if (!GetProcessTimes(process, out start, out exited, out kernel, out user)) { return false; }
            if (Math.Abs(start - created) > 10000) { return false; }
            if (Owner(process) != sid) { return false; }
            if (!TerminateProcess(process, 1) && WaitForSingleObject(process, 0) != 0) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "could not stop " + path + " (process " + pid + ")");
            }
            WaitForSingleObject(process, 5000);
            return true;
        } finally {
            CloseHandle(process);
        }
    }

    static string Owner(IntPtr process) {
        IntPtr token;
        if (!OpenProcessToken(process, TokenQuery, out token)) { return null; }
        try {
            int needed;
            GetTokenInformation(token, 1, IntPtr.Zero, 0, out needed);
            if (needed <= 0) { return null; }
            IntPtr info = Marshal.AllocHGlobal(needed);
            try {
                if (!GetTokenInformation(token, 1, info, needed, out needed)) { return null; }
                // TOKEN_USER starts with a SID_AND_ATTRIBUTES whose first field is the SID.
                return new SecurityIdentifier(Marshal.ReadIntPtr(info)).Value;
            } finally {
                Marshal.FreeHGlobal(info);
            }
        } finally {
            CloseHandle(token);
        }
    }
}
'@
$Me = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
foreach ($candidate in Get-CimInstance Win32_Process -Filter "Name = 'latch-host.exe'") {
    if (!$candidate.CreationDate) { continue }
    [void][LatchInstallStop]::Stop([uint32]$candidate.ProcessId, $DestExe, $Me, $candidate.CreationDate.ToFileTimeUtc())
}
Get-Process brolink-host -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
if ((Test-Path $OldDir) -and -not (Test-Path (Join-Path $DestDir "host.toml"))) {
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
    Get-ChildItem -LiteralPath $OldDir -Force | Where-Object { $_.Name -notmatch '\.(exe|zip)(\.old|\.new)?$' -and $_.Name -ne 'engine' } |
        ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $DestDir -Recurse -Force }
    Write-Host "Settings carried over from the previous install."
}
New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
Copy-Item $Exe $DestExe -Force
Write-Host "Installed: $DestExe"
# Latch 4 kept its engine's archive (about 25 MB) and the scripts that
# started the engine in this folder. Latch 5 has no separate engine; the
# engine itself was in Program Files, which setup's retire step removes.
foreach ($old in 'Sunshine-Windows-AMD64-lite.zip', 'take-over-engine.ps1', 'take-over-engine.vbs', 'take-over-engine.cmd') {
    Remove-Item -LiteralPath (Join-Path $DestDir $old) -Force -ErrorAction SilentlyContinue
}
$Wsh = New-Object -ComObject WScript.Shell
$Desktop = [Environment]::GetFolderPath("Desktop")
$StartMenu = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
New-Item -ItemType Directory -Force -Path $StartMenu | Out-Null
foreach ($folder in @($Desktop, $StartMenu)) {
    Remove-Item -LiteralPath (Join-Path $folder "BroLink Host.lnk") -Force -ErrorAction SilentlyContinue
    $Lnk = $Wsh.CreateShortcut((Join-Path $folder "Latch Host.lnk"))
    $Lnk.TargetPath = $DestExe
    $Lnk.WorkingDirectory = $DestDir
    $Lnk.Description = "Your PC, from your Mac"
    $Lnk.Save()
}

# The background service at logon; the app offers the same toggle.
$Run = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
# A profile that never had a login item has no Run key.
if (-not (Test-Path $Run)) { New-Item -Path $Run | Out-Null }
Remove-ItemProperty -Path $Run -Name "BroLinkHost" -ErrorAction SilentlyContinue
New-ItemProperty -Path $Run -Name "LatchHost" -Value "`"$DestExe`" --background" -PropertyType String -Force | Out-Null

# Settings > Apps lists it, and its Uninstall runs latch-host --uninstall.
$Apps = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Latch"
New-Item -Path $Apps -Force | Out-Null
$Version = (Get-Item -LiteralPath $DestExe).VersionInfo.ProductVersion
foreach ($v in @(
        @('DisplayName', 'Latch'),
        @('DisplayIcon', $DestExe),
        @('DisplayVersion', "$Version"),
        @('InstallLocation', $DestDir),
        @('UninstallString', "`"$DestExe`" --uninstall")
    )) {
    New-ItemProperty -Path $Apps -Name $v[0] -Value $v[1] -PropertyType String -Force | Out-Null
}
foreach ($flag in 'NoModify', 'NoRepair') {
    New-ItemProperty -Path $Apps -Name $flag -Value 1 -PropertyType DWord -Force | Out-Null
}

if (Test-Path $OldDir) {
    Remove-Item -LiteralPath $OldDir -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $OldDir) { Write-Host "Could not remove the previous install folder completely; it is safe to delete." }
}

Write-Host "Shortcut: Desktop\Latch Host.lnk"
Write-Host "Open Latch and click 'Set up as administrator' on the Sharing tab once."
Write-Host "If you already ran setup from the unzipped copy, run it once more from this install so the firewall allows $DestExe."
if ((Get-Process -Id $PID).SessionId -eq 0) {
    # The task runs as this user, unelevated like the Run key, in the session
    # they are signed in to. Task Scheduler's defaults would stop the host
    # after 72 hours, or on battery, and run it at below-normal priority.
    $TaskName = "LatchHostStart"
    $up = $false
    try {
        $action = New-ScheduledTaskAction -Execute $DestExe -Argument "--background" -WorkingDirectory $DestDir
        # USERDOMAIN is not set in an SSH session; the token's own name is.
        $me = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
        $principal = New-ScheduledTaskPrincipal -UserId $me -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 5
        Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $TaskName
        foreach ($i in 1..20) {
            Start-Sleep -Milliseconds 750
            try {
                Invoke-RestMethod -Uri "http://127.0.0.1:47850/v1/status" -UseBasicParsing -TimeoutSec 2 | Out-Null
                $up = $true
                break
            } catch {}
        }
    } catch {
        Write-Host "Could not start Latch Host in the signed-in session: $_"
    } finally {
        # Deleting a task does not terminate its running instances. Remove
        # the temporary registration even when startup failed or timed out.
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    }
    if ($up) {
        Write-Host "Latch Host is running in the signed-in user's session."
    } else {
        Write-Host "Nobody is signed in to this PC's desktop: Latch Host starts at the next sign-in."
    }
} elseif ($NoStart) {
    Start-Process -FilePath $DestExe -ArgumentList "--background" -WorkingDirectory $DestDir
} else {
    Start-Process -FilePath $DestExe -WorkingDirectory $DestDir
}
