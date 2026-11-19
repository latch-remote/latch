//! The administrator step (firewall, wake, virtual-display modes) and autostart.
#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use latch_core::config::data_dir;
use latch_core::CONTROL_PORT;
use std::path::{Path, PathBuf};

pub struct Plan<'a> {
    pub elevated_input: bool,
    pub exe: &'a Path,
    pub adapter: &'a str,
    pub adapter_description: &'a str,
}

/// The setup transcript. On Windows the elevated step writes it beside
/// the copy of Latch it runs from, in `%ProgramFiles%\Latch\Setup`:
/// never under a folder the signed-in user (or whoever started the
/// elevated step) can redirect.
pub fn log_path() -> Option<PathBuf> {
    if cfg!(windows) {
        let pf = std::env::var_os("ProgramFiles").filter(|d| !d.is_empty())?;
        return Some(PathBuf::from(pf).join(SETUP_DIR).join("setup.log"));
    }
    data_dir().ok().map(|d| d.join("setup.log"))
}

/// Under `%ProgramFiles%`, which only administrators can change.
const SETUP_DIR: &str = r"Latch\Setup";

/// Windows' system folder (`System32`), from Windows itself rather than
/// an environment variable the caller chose.
#[cfg(any(windows, test))]
fn system_dir() -> PathBuf {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
        }
        let mut buffer = [0u16; 32768];
        // SAFETY: the buffer has the length passed to Windows.
        let n = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        assert!(
            n > 0 && n < buffer.len(),
            "Windows system directory unavailable"
        );
        PathBuf::from(String::from_utf16_lossy(&buffer[..n]))
    }
    #[cfg(not(windows))]
    PathBuf::from(r"C:\Windows\System32")
}

/// Windows' own PowerShell, by full path. A bare `powershell` is looked
/// for in the running executable's folder first, which for Latch is one
/// the user can write to.
#[cfg(any(windows, test))]
fn powershell_exe() -> PathBuf {
    system_dir().join(r"WindowsPowerShell\v1.0\powershell.exe")
}

/// `program` (a file in [`system_dir`]) in an environment the caller
/// cannot steer, as setup needs it whenever it may run elevated: `PATH` is
/// only the system folder, PowerShell loads modules only from Windows' own
/// folder (Windows PowerShell 5.1 otherwise looks in the user's Documents
/// first, which their unelevated processes can write to), and the working
/// directory is the system folder. No window.
#[cfg(any(windows, test))]
fn trusted_command(program: &Path) -> std::process::Command {
    let system = system_dir();
    let mut command = std::process::Command::new(program);
    command
        .env("PATH", &system)
        .env(
            "PSModulePath",
            system.join(r"WindowsPowerShell\v1.0\Modules"),
        )
        .current_dir(&system);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Windows' PowerShell as [`trusted_command`] starts it, without the
/// user's profile scripts and without prompts.
#[cfg(any(windows, test))]
fn powershell_command() -> std::process::Command {
    let mut command = trusted_command(&powershell_exe());
    command.args(["-NoProfile", "-NonInteractive"]);
    command
}

/// Windows' `schtasks.exe`, as [`trusted_command`] starts it.
#[cfg(any(windows, test))]
fn schtasks_command() -> std::process::Command {
    trusted_command(&system_dir().join("schtasks.exe"))
}

/// Whether the task `name` is registered and this user may see it, as
/// `schtasks.exe` says (no PowerShell module is loaded to ask).
#[cfg(windows)]
pub(crate) fn task_registered(name: &str) -> bool {
    schtasks_command()
        .args(["/Query", "/TN", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// What the administrator step exits with when it never ran setup.
#[cfg_attr(not(windows), allow(dead_code))]
mod exit {
    /// Windows' "the operation was cancelled by the user": the UAC prompt
    /// was declined, or could not be shown.
    pub const DECLINED: i32 = 1223;
    /// Latch could not be copied into `%ProgramFiles%\Latch\Setup`.
    pub const COPY_FAILED: i32 = 3;
    /// The copy is not the file this window was started from.
    pub const CHANGED: i32 = 4;
}

/// What the window says when the administrator prompt was turned down.
pub const ADMIN_DECLINED: &str = "Setup needs an administrator's approval, and Windows' prompt was declined. Try again and choose Yes.";

/// Why the administrator step failed, from its exit code.
#[cfg_attr(not(windows), allow(dead_code))]
fn elevation_error(code: Option<i32>) -> String {
    match code {
        Some(exit::DECLINED) => ADMIN_DECLINED.into(),
        Some(exit::COPY_FAILED) => {
            "setup could not copy Latch into Program Files to run it as administrator".into()
        }
        Some(exit::CHANGED) => "latch-host.exe changed on disk while Windows asked for approval, so setup did not run it. Open Latch again and retry.".into(),
        _ => "setup stopped part way; the setup log shows the last step reached".into(),
    }
}

/// The elevated first step, run by Windows' own PowerShell after the UAC
/// prompt. It copies `program` into `%ProgramFiles%\Latch\Setup`, checks
/// the copy is the file the window hashed (`sha256`), and runs the copy as
/// `--setup-elevated`. The elevated Latch therefore loads, runs and writes
/// nothing in a folder a user can change: the install folder under
/// `%LOCALAPPDATA%` is the user's, and an unzipped copy may sit in one
/// every user can write to. `program` is only named in the firewall rules.
///
/// `user_sid` is the user who asked for setup, unelevated: whoever
/// approves the prompt may be another administrator, and it is the asking
/// user whose Latch the input helper is for. `elevated_input` is that
/// user's setting, which the elevated step cannot read from another
/// account's profile.
#[cfg(any(windows, test))]
fn stage_script(program: &str, sha256: &str, user_sid: &str, elevated_input: bool) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$program = {program}
$sha256 = {sha256}
$userSid = {user_sid}
$elevatedInput = {elevated_input}
{protection}
$pins = @{{}}
try {{
  try {{
    # Everything the elevated copy may load sits in this folder. One that
    # anyone else could change is made again, empty and protected.
    $dir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) '{dir}'
    Reset-ProtectedFolder $dir ([Environment]::GetFolderPath('ProgramFiles')) $pins
    $copy = Join-Path $dir 'latch-host.exe'
    if (Test-Path -LiteralPath $copy) {{ Remove-Item -LiteralPath $copy -Force }}
    [IO.File]::Copy($program, $copy)
    if (Test-Protected $copy) {{ Repair-Protected $copy }}
  }} catch {{ exit {copy_failed} }}
if ((Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash -ne $sha256) {{
    Remove-Item -LiteralPath $copy -Force -ErrorAction SilentlyContinue
    exit {changed}
}}
# Started directly rather than through the shell, which would ask again
# about a downloaded file this window already runs.
$start = New-Object System.Diagnostics.ProcessStartInfo $copy
$start.Arguments = '--setup-elevated --program "' + $program + '" --for-user ' + $userSid + ' --elevated-input ' + $elevatedInput
$start.UseShellExecute = $false
$start.WorkingDirectory = $dir
$p = [System.Diagnostics.Process]::Start($start)
$p.WaitForExit()
Remove-Item -LiteralPath $copy -Force -ErrorAction SilentlyContinue
exit $p.ExitCode
}} finally {{
    foreach ($handle in $pins.Values) {{ $handle.Dispose() }}
}}
"#,
        program = ps_data(program),
        protection = protection_ps(),
        dir = SETUP_DIR,
        sha256 = ps_data(sha256),
        user_sid = ps_data(user_sid),
        elevated_input = ps_data(&elevated_input.to_string()),
        copy_failed = exit::COPY_FAILED,
        changed = exit::CHANGED,
    )
}

/// `scripts/setup-protection.ps1` as setup embeds it: without its comment
/// lines, which would only lengthen the elevated command line (Windows
/// caps it at 32767 characters, and the script travels base64-encoded).
fn protection_ps() -> String {
    include_str!("../../../scripts/setup-protection.ps1")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Only the checks (`Test-Protected`, `Assert-Protected`,
/// `Assert-ProtectedTree`), for a script that repairs nothing and must fit
/// the command line with room to spare, such as uninstall's.
#[cfg(any(windows, test))]
fn protection_checks_ps() -> String {
    let full = protection_ps();
    let end = full
        .find("function Repair-Protected")
        .expect("the repair functions follow the checks");
    full[..end].to_owned()
}

/// The unelevated launcher: Windows' PowerShell, elevated through UAC, runs
/// [`stage_script`]. The script travels on the command line, which UAC
/// fixes when it is approved, rather than in a file the user could edit.
#[cfg(any(windows, test))]
fn launch_script(stage: &str) -> String {
    // Already elevated (an administrator's SSH session, a window started as
    // administrator): no prompt to show, and none could be shown in
    // session 0.
    format!(
        "$encoded = '{encoded}'; $ps = {ps}; if (([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{ & $ps -NoProfile -NonInteractive -EncodedCommand $encoded; exit $LASTEXITCODE }}; try {{ $p = Start-Process -FilePath $ps -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList ('-NoProfile -NonInteractive -EncodedCommand ' + $encoded) -ErrorAction Stop }} catch {{ exit {declined} }}; if ($null -eq $p) {{ exit {declined} }}; exit $p.ExitCode",
        ps = ps_data(&powershell_exe().display().to_string()),
        encoded = encoded_command(stage),
        declined = exit::DECLINED,
    )
}

/// A script as `-EncodedCommand` takes it: base64 of its UTF-16LE text,
/// which no quoting on the way there can change.
#[cfg(any(windows, test))]
fn encoded_command(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&utf16)
}

/// Standard base64 with padding, for `-EncodedCommand`.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The setup script. `user_sid`: the user setup is for (see
/// [`stage_script`]), already checked.
pub fn script(p: &Plan<'_>, user_sid: &str) -> String {
    let adapter = if p.adapter.is_empty() {
        "Step \"Wake-on-LAN: adapter unknown, skipped\"\n".to_string()
    } else {
        format!(
            r#"$wakeAdapter = {adapter}
$wakeDevice = {desc}
Step "Enabling Wake-on-LAN on '$wakeAdapter'"
try {{
    Set-NetAdapterPowerManagement -Name $wakeAdapter -WakeOnMagicPacket Enabled -ErrorAction Stop
}} catch {{ Write-Output "  cmdlet failed ($_); the driver keywords below still apply" }}
# The NDIS keywords are what the driver reads: magic packet from sleep, from
# modern standby, and (Realtek's own keyword) from a full shutdown. ARP and
# NS offload keep the card answering for the PC's address while it sleeps,
# which is what lets a unicast wake packet reach it through a router.
$g = (Get-NetAdapter -Name $wakeAdapter -ErrorAction SilentlyContinue).InterfaceGuid
$k = Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{{4d36e972-e325-11ce-bfc1-08002be10318}}' -ErrorAction SilentlyContinue | Where-Object {{ (Get-ItemProperty $_.PSPath -Name NetCfgInstanceId -ErrorAction SilentlyContinue).NetCfgInstanceId -eq $g }} | Select-Object -First 1
if ($k) {{
    $changed = $false
    foreach ($kw in '*WakeOnMagicPacket', '*ModernStandbyWoLMagicPacket', 'S5WakeOnLan', '*PMARPOffload', '*PMNSOffload') {{
        if ((Get-ItemProperty $k.PSPath -ErrorAction SilentlyContinue).$kw -ne '1') {{
            Set-ItemProperty $k.PSPath -Name $kw -Value '1' -Type String
            $changed = $true
        }}
    }}
    if ($changed) {{ Restart-NetAdapter -Name $wakeAdapter -ErrorAction SilentlyContinue }}
}} else {{ Write-Output "  no class key for the adapter; keywords unchanged" }}
try {{ powercfg /deviceenablewake $wakeDevice | Out-Null }} catch {{ Write-Output "  powercfg: $_" }}
"#,
            adapter = ps_data(p.adapter),
            desc = ps_data(p.adapter_description)
        )
    };
    format!(
        r#"# Latch sharing setup. Generated by latch-host.
$ErrorActionPreference = 'Continue'
function Step($m) {{ Write-Output "[$(Get-Date -Format HH:mm:ss)] $m" }}
$setupFailed = $false
{protection}
Step "Latch setup started"
{retire}
Step "Opening TCP and UDP {port} to the tailnet for Latch"
# The first time a new latch-host.exe listens, Windows asks whether to allow
# it; unanswered (nobody at the PC, an install over SSH) that becomes a pair
# of inbound Block rules for the program, and a Block rule wins over every
# Allow below. Only rules for this very executable are removed.
$latchExe = {exe}
Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue | Where-Object {{ $_.Program -ieq $latchExe }} | Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object {{ $_.Action -eq 'Block' -and $_.Direction -eq 'Inbound' }} | Remove-NetFirewallRule -ErrorAction SilentlyContinue
foreach ($rule in 'Latch Host', 'Latch Media') {{ netsh advfirewall firewall delete rule name="$rule" | Out-Null }}
# Tailscale's addresses only: its IPv4 range and its IPv6 one.
netsh advfirewall firewall add rule name="Latch Host" dir=in action=allow protocol=TCP localport={port} remoteip={tailnet} "program=$latchExe" | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
netsh advfirewall firewall add rule name="Latch Media" dir=in action=allow protocol=UDP localport={port} remoteip={tailnet} "program=$latchExe" | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
Step "Opening UDP 9 to check the wake path while this PC is awake"
netsh advfirewall firewall delete rule name="Latch wake" | Out-Null
netsh advfirewall firewall add rule name="Latch wake" dir=in action=allow protocol=UDP localport=9 "program=$latchExe" | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
{input_helper}
{virtual_display}
Step "Recording the power settings this PC had before Latch"
{record}
{gamepad_driver}
Step "Turning Fast Startup off so a shut-down PC can wake"
try {{ Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0 -Type DWord -ErrorAction Stop }} catch {{ Write-Output "  Fast Startup: $_"; $setupFailed = $true }}
Step "Keeping this PC awake when plugged in so Tailscale stays reachable"
powercfg /change standby-timeout-ac 0
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
powercfg /change hibernate-timeout-ac 0
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
{adapter}
if ($setupFailed) {{ Step "Some setup steps failed. See the lines above."; exit 1 }}
if ($vddModesUnchanged) {{ Step "Latch setup finished (virtual display modes unchanged)" }}
else {{ Step "Latch setup finished" }}
exit 0
"#,
        protection = protection_ps(),
        retire = crate::legacy::retire_windows_ps(),
        port = CONTROL_PORT,
        tailnet = "100.64.0.0/10,fd7a:115c:a1e0::/48",
        exe = ps_data(&p.exe.display().to_string()),
        virtual_display = crate::virtual_display::setup_ps(),
        input_helper = input_helper_ps(p.elevated_input, user_sid),
        record = record_power_ps(),
        gamepad_driver = gamepad_driver_ps(),
    )
}

/// The ViGEmBus release setup installs for viewers' gamepads (the last one;
/// the project has ended, the driver is signed by Microsoft's hardware
/// program), and its SHA-256, so only that very file ever runs.
const VIGEM_URL: &str =
    "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe";
const VIGEM_SHA256: &str = "89220A7865076B342892F98865F3499FB7C4CFD673159E89D352C360FD014C6A";
/// Under [`SAVED_KEY`]: setup installed ViGEmBus, so `--uninstall` removes it.
const VIGEM_MARK: &str = "ViGEmBusInstalled";
/// Finds ViGEmBus's entry in Programs and Features, if it is installed.
const VIGEM_ENTRY: &str = r#"Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like 'ViGEm Bus Driver*' } | Select-Object -First 1"#;

/// Setup's step for gamepads: installs ViGEmBus unless something already
/// did, from the pinned release, downloaded into setup's administrators-only
/// folder so nothing can swap the file between the check and the run. The
/// installer runs silently and never restarts Windows (an upgrade over an
/// older ViGEmBus would otherwise force one); if it says a
/// restart is needed, the step says so. A failure here leaves gamepads off
/// and fails nothing else.
fn gamepad_driver_ps() -> String {
    format!(
        r#"Step "Installing the ViGEmBus gamepad driver, so a viewer's gamepads reach games"
try {{
    $vigem = {entry}
    if ($vigem -or (Get-Service -Name ViGEmBus -ErrorAction SilentlyContinue)) {{
        Write-Output "  already installed ($($vigem.DisplayVersion))"
    }} else {{
        $vigemDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch\Setup'
        Assert-ProtectedTree $vigemDir
        $vigemSetup = Join-Path $vigemDir 'ViGEmBus-setup.exe'
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile $vigemSetup -ErrorAction Stop
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $vigemSetup).Hash
        if ($hash -ne '{sha256}') {{
            Remove-Item -LiteralPath $vigemSetup -Force -ErrorAction SilentlyContinue
            throw "the download's SHA-256 is $hash, not the one Latch knows"
        }}
        Write-Output "  signature: $((Get-AuthenticodeSignature -LiteralPath $vigemSetup).Status)"
        $p = Start-Process -FilePath $vigemSetup -ArgumentList '/qn', '/norestart', 'REBOOT=ReallySuppress' -Wait -PassThru
        Remove-Item -LiteralPath $vigemSetup -Force -ErrorAction SilentlyContinue
        if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {{ throw "the installer exited with $($p.ExitCode)" }}
        # Without -Force: on an existing key it would wipe the recorded power settings.
        if (-not (Test-Path -LiteralPath $latchSaved)) {{ New-Item -Path $latchSaved -ErrorAction Stop | Out-Null }}
        New-ItemProperty -LiteralPath $latchSaved -Name '{mark}' -Value '1' -PropertyType String -Force -ErrorAction Stop | Out-Null
        if ($p.ExitCode -eq 3010) {{ Write-Output "  installed; gamepads work after Windows next restarts" }} else {{ Write-Output "  installed" }}
    }}
}} catch {{ Write-Output "  gamepad driver: $_ (viewers' gamepads stay off; nothing else depends on it)" }}"#,
        entry = VIGEM_ENTRY,
        url = VIGEM_URL,
        sha256 = VIGEM_SHA256,
        mark = VIGEM_MARK,
    )
}

/// The task that runs the input helper as SYSTEM, for windows that run as
/// administrator and the secure desktop (see `latch_engine::windows`'s
/// `elevated` module).
const INPUT_TASK: &str = "LatchInput";
/// Windows' policy that lets services send Ctrl+Alt+Delete (SendSAS).
const SAS_POLICY_KEY: &str = r"HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
/// Setup turned that policy on (it was unset), so it turns it off again.
const SAS_MARK_KEY: &str = r"HKLM:\SOFTWARE\Latch\Input";

/// Takes back the SendSAS policy when setup set it, and then only if it
/// still holds exactly what setup wrote (the DWORD 1): an administrator
/// who changed it since keeps their value.
fn sas_policy_removal_ps() -> String {
    format!(
        r#"if ((Get-ItemProperty -LiteralPath '{mark}' -Name SasPolicyAdded -ErrorAction SilentlyContinue).SasPolicyAdded -eq '1') {{
    $sasKey = Get-Item -LiteralPath '{policy}' -ErrorAction SilentlyContinue
    if ($sasKey -and ($sasKey.GetValueNames() -contains 'SoftwareSASGeneration') -and $sasKey.GetValueKind('SoftwareSASGeneration') -eq [Microsoft.Win32.RegistryValueKind]::DWord -and $sasKey.GetValue('SoftwareSASGeneration') -eq 1) {{
        Remove-ItemProperty -LiteralPath '{policy}' -Name SoftwareSASGeneration -ErrorAction SilentlyContinue
    }}
    Remove-Item -LiteralPath '{mark}' -Recurse -Force -ErrorAction SilentlyContinue
}}"#,
        mark = SAS_MARK_KEY,
        policy = SAS_POLICY_KEY,
    )
}

/// Setup's step for the input helper. Windows ignores input an unelevated
/// Latch sends while an elevated window is in front, and only SYSTEM can
/// show or control the lock screen, the sign-in screen and administrator
/// prompts. So a stream starts this task, which runs a copy of this checked
/// file as LocalSystem; that copy starts its agent in the sessions where
/// the approved user runs the host recorded in `host-path.txt`, and serves
/// only that user's host. Turning this on therefore lets that user's Latch
/// (and whatever can run as that user and control it) answer administrator
/// prompts on this PC.
///
/// `user_sid` is the approved user, recorded in `user-sid.txt`: the one
/// who asked for setup, not the administrator who approved the prompt.
/// The copy sits in `%ProgramFiles%\Latch\Input`, which only
/// administrators can change and which is made again when anything in it
/// is not theirs, and is replaced on every setup. The task's security lets
/// the approved user start it, and only administrators change it.
fn input_helper_ps(enabled: bool, user_sid: &str) -> String {
    if !enabled {
        return format!(
            r#"Step "Removing administrator input helper"
try {{
    Stop-ScheduledTask -TaskName '{INPUT_TASK}' -ErrorAction SilentlyContinue
    if (Get-ScheduledTask -TaskName '{INPUT_TASK}' -ErrorAction SilentlyContinue) {{
        Unregister-ScheduledTask -TaskName '{INPUT_TASK}' -Confirm:$false -ErrorAction Stop
    }}
    # The agents run apart from the task: stop them too, and remove the
    # copy, so no host keeps asking for a helper that is off.
    $inputDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch\Input'
    $inputExe = Join-Path $inputDir 'latch-host.exe'
    Get-Process -Name 'latch-host' -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -ieq $inputExe }} | Stop-Process -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $inputDir) {{
        Start-Sleep -Milliseconds 500
        Assert-ProtectedTree (Split-Path -Parent $inputDir)
        Remove-Tree $inputDir
    }}
}} catch {{ Write-Output "  input helper: $_"; $setupFailed = $true }}
{sas}"#,
            sas = sas_policy_removal_ps(),
        );
    }
    format!(
        r#"Step "Letting a viewer control windows that run as administrator, the lock screen and administrator prompts"
try {{
    # The user who asked for setup; whoever approved the prompt may be
    # another administrator. Only this user's Latch may use the helper.
    $approvedSid = {user_sid}
    $approvedName = ([Security.Principal.SecurityIdentifier]$approvedSid).Translate([Security.Principal.NTAccount]).Value
    Write-Output "  for $approvedName, whose Latch can then answer administrator prompts"
    $inputSource = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch\Setup\latch-host.exe'
    Assert-ProtectedTree $inputSource
    $inputDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch\Input'
    $inputExe = Join-Path $inputDir 'latch-host.exe'
    # A running helper holds the file it runs from.
    Stop-ScheduledTask -TaskName '{task}' -ErrorAction SilentlyContinue
    Get-Process -Name 'latch-host' -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -ieq $inputExe }} | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    # SYSTEM runs what is in this folder. A tree with unsafe contents
    # is moved aside and replaced with a protected one.
    Reset-ProtectedFolder $inputDir
    Copy-Item -LiteralPath $inputSource -Destination $inputExe -Force -ErrorAction Stop
    $hostRecord = Join-Path $inputDir 'host-path.txt'
    Set-Content -LiteralPath $hostRecord -Value $latchExe -Encoding UTF8 -ErrorAction Stop
    $userRecord = Join-Path $inputDir 'user-sid.txt'
    Set-Content -LiteralPath $userRecord -Value $approvedSid -Encoding ASCII -ErrorAction Stop
    foreach ($written in $inputExe, $hostRecord, $userRecord) {{ if (Test-Protected $written) {{ Repair-Protected $written }} }}
    if (-not (Test-ProtectedContents $inputDir)) {{ throw "$inputDir is not protected" }}
    $action = New-ScheduledTaskAction -Execute $inputExe -Argument '{arg}' -WorkingDirectory $inputDir
    # LocalSystem, the only account that can reach the secure desktop.
    $principal = New-ScheduledTaskPrincipal -UserId '{system}' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -Priority 4
    Register-ScheduledTask -TaskName '{task}' -Action $action -Principal $principal -Settings $settings -Force -ErrorAction Stop | Out-Null
    # SYSTEM and administrators may change the task; the approved user may
    # only read and start it. Nobody else may even start it.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $scheduler.GetFolder('\').GetTask('{task}').SetSecurityDescriptor("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;$approvedSid)", 0)
    # Ctrl+Alt+Delete from a viewer: Windows lets a service (the helper runs
    # as SYSTEM) send it only under this policy. Setup sets it when unset.
    # It marks that first, so a setup that stops part way still takes it
    # back, and takes the mark back if the write fails; turning this off or
    # uninstalling removes the value only while it is still setup's.
    if ($null -eq (Get-ItemProperty -LiteralPath '{policy}' -Name SoftwareSASGeneration -ErrorAction SilentlyContinue)) {{
        if (-not (Test-Path -LiteralPath '{mark}')) {{ New-Item -Path '{mark}' -Force -ErrorAction Stop | Out-Null }}
        New-ItemProperty -LiteralPath '{mark}' -Name SasPolicyAdded -Value '1' -PropertyType String -Force -ErrorAction Stop | Out-Null
        try {{
            New-ItemProperty -LiteralPath '{policy}' -Name SoftwareSASGeneration -Value 1 -PropertyType DWord -Force -ErrorAction Stop | Out-Null
        }} catch {{
            Remove-ItemProperty -LiteralPath '{mark}' -Name SasPolicyAdded -ErrorAction SilentlyContinue
            throw
        }}
    }} elseif ((Get-ItemProperty -LiteralPath '{policy}').SoftwareSASGeneration -notin 1, 3) {{
        Write-Output "  Ctrl+Alt+Delete from a viewer stays off: the SoftwareSASGeneration policy is set to something else"
    }}
}} catch {{ Write-Output "  input helper: $_"; $setupFailed = $true }}"#,
        user_sid = ps_data(user_sid),
        task = INPUT_TASK,
        arg = "--input-helper",
        system = "S-1-5-18",
        policy = SAS_POLICY_KEY,
        mark = SAS_MARK_KEY,
    )
}

/// Where setup keeps what it changed, for `--uninstall` to put back.
/// Administrators' only, like everything setup writes.
const SAVED_KEY: &str = r"HKLM:\SOFTWARE\Latch\Setup";

/// The AC sleep and hibernate timeouts and Fast Startup as they were before
/// Latch's first setup, under [`SAVED_KEY`]. A later setup records nothing:
/// by then the values are its own. powercfg's labels are translated, so the
/// AC value is read as the second-to-last hex number it prints (the DC
/// value is the last).
fn record_power_ps() -> String {
    format!(
        r#"$latchSaved = '{key}'
if (-not (Test-Path -LiteralPath $latchSaved)) {{
    try {{
        $scheme = [regex]::Match((powercfg /getactivescheme | Out-String), '[0-9a-fA-F]{{8}}(-[0-9a-fA-F]{{4}}){{3}}-[0-9a-fA-F]{{12}}').Value
        if (-not $scheme) {{ throw 'Could not read the active power plan' }}
        $values = @{{ PowerScheme = $scheme }}
        $hb = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -ErrorAction SilentlyContinue).HiberbootEnabled
        $values.HiberbootEnabled = if ($null -eq $hb) {{ 'absent' }} else {{ [string]$hb }}
        foreach ($s in @(@('StandbyTimeoutAC', 'STANDBYIDLE'), @('HibernateTimeoutAC', 'HIBERNATEIDLE'))) {{
            $hex = [regex]::Matches((powercfg /query SCHEME_CURRENT SUB_SLEEP $s[1] | Out-String), '0x[0-9a-fA-F]{{8}}')
            if ($LASTEXITCODE -ne 0 -or $hex.Count -lt 2) {{ throw 'Could not read the power timeout' }}
            $values[$s[0]] = [string][Convert]::ToUInt32($hex[$hex.Count - 2].Value, 16)
        }}
        New-Item -Path $latchSaved -Force -ErrorAction Stop | Out-Null
        foreach ($name in $values.Keys) {{
            New-ItemProperty -LiteralPath $latchSaved -Name $name -Value $values[$name] -PropertyType String -Force -ErrorAction Stop | Out-Null
        }}
    }} catch {{
        Remove-Item -LiteralPath $latchSaved -Recurse -Force -ErrorAction SilentlyContinue
        Write-Output "  power settings were not saved: $_; leaving them unchanged"
        exit 1
    }}
}} else {{ Write-Output "  kept from an earlier setup" }}"#,
        key = SAVED_KEY
    )
}

pub fn run(p: &Plan<'_>) -> Result<()> {
    #[cfg(not(windows))]
    {
        crate::unix_setup::run(p)
    }
    #[cfg(windows)]
    {
        static WARNED: parking_lot::Mutex<Option<std::time::Instant>> =
            parking_lot::Mutex::new(None);
        let viewer = latch_core::http::get_json::<latch_core::api::Status>(
            ("127.0.0.1", CONTROL_PORT),
            "/v1/status",
            std::time::Duration::from_secs(2),
        )
        .is_ok_and(|s| s.streamer.viewer);
        {
            let now = std::time::Instant::now();
            let mut warned = WARNED.lock();
            if refuse_during_stream(viewer, *warned, now) {
                *warned = Some(now);
                anyhow::bail!(SETUP_DURING_STREAM);
            }
            *warned = None;
        }
        // Pinned now: the elevated step runs only a copy of these bytes.
        let sha256 = latch_core::update::sha256_hex(
            &std::fs::read(p.exe).with_context(|| format!("read {}", p.exe.display()))?,
        );
        // The user the input helper is for: this one, unelevated. The
        // prompt may be approved with another administrator's account.
        let user_sid = latch_engine::windows::current_user_sid()
            .context("read which user is setting up Latch")?;
        let launch = launch_script(&stage_script(
            &p.exe.display().to_string(),
            &sha256,
            &user_sid,
            p.elevated_input,
        ));
        let status = powershell_command()
            .args(["-Command", &launch])
            .status()
            .context("launch elevated Latch Host")?;
        anyhow::ensure!(status.success(), elevation_error(status.code()));
        Ok(())
    }
}

/// UAC needs someone at the PC even though a stream is already open.
pub const SETUP_DURING_STREAM: &str = "A stream is running. Windows asks for an administrator on this PC's own screen. Click again within two minutes to run setup with someone at the PC, or wait until the stream ends.";
#[cfg_attr(not(windows), allow(dead_code))]
const SETUP_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether a setup click is turned down: a viewer is connected, and this is
/// not the second click within [`SETUP_CONFIRM_WINDOW`] of the warning.
#[cfg_attr(not(windows), allow(dead_code))]
fn refuse_during_stream(
    viewer: bool,
    warned: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    viewer && !warned.is_some_and(|at| now.duration_since(at) < SETUP_CONFIRM_WINDOW)
}

/// Run a read-only PowerShell probe and return what it printed.
#[cfg(windows)]
pub(crate) fn powershell(script: &str) -> Result<String> {
    let out = powershell_command()
        .args(["-Command", script])
        .stdin(std::process::Stdio::null())
        .output()
        .context("run powershell")?;
    anyhow::ensure!(
        out.status.success(),
        "powershell exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A PowerShell expression that decodes data without parsing it as source.
fn ps_data(s: &str) -> String {
    format!(
        "[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))",
        base64(s.as_bytes())
    )
}

/// Hold each directory from the local volume root before opening its
/// children. The caller also checks the owners and ACLs before writing.
#[cfg(windows)]
fn pin_setup_path(path: &Path) -> Result<Vec<std::fs::File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Prefix};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    anyhow::ensure!(
        matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
        "setup requires a local path"
    );
    let mut pins = Vec::new();
    let ancestors = path.ancestors().collect::<Vec<_>>();
    for dir in ancestors.into_iter().rev() {
        let pin = std::fs::OpenOptions::new()
            .access_mode((FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES).0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(dir)
            .with_context(|| format!("pin setup directory {}", dir.display()))?;
        let info = pin.metadata()?;
        anyhow::ensure!(
            info.is_dir() && info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
            "unsafe setup directory {}",
            dir.display()
        );
        pins.push(pin);
    }
    Ok(pins)
}

/// Generate and run the setup script. Called from `--setup-elevated`,
/// already elevated: with `program`, by the copy [`stage_script`] made in
/// `%ProgramFiles%\Latch\Setup`, which also passes the user who asked
/// (`for_user`) and their `elevated_input` setting; without, by an
/// administrator's own session (SSH), which stages this file the same way
/// first, for that administrator.
pub fn run_as_admin(
    program: Option<&Path>,
    for_user: Option<&str>,
    elevated_input: Option<bool>,
) -> Result<()> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().context("own path")?;
        let Some(program) = program else {
            let sha256 = latch_core::update::sha256_hex(&std::fs::read(&exe)?);
            let user_sid = latch_engine::windows::current_user_sid()?;
            let stage = stage_script(
                &exe.display().to_string(),
                &sha256,
                &user_sid,
                crate::config::HostConfig::load_read_only().elevated_input,
            );
            let status = powershell_command()
                .arg("-EncodedCommand")
                .arg(encoded_command(&stage))
                .status()
                .context("run the setup copy")?;
            anyhow::ensure!(status.success(), elevation_error(status.code()));
            return Ok(());
        };
        // Everything below is written beside this copy, so it has to be
        // the one in Program Files and not a file a user put anywhere.
        let dir = exe.parent().context("own folder")?.to_path_buf();
        let _pins = pin_setup_path(&dir)?;
        let checked = powershell(&format!(
            "{}\n$dir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch\\Setup'; Assert-ProtectedTree $dir; $log = Join-Path $dir 'setup.log'; if ((Test-Path -LiteralPath $log) -and (Test-Protected $log)) {{ Remove-Item -LiteralPath $log -Force }}; Write-Output $dir",
            protection_ps()
        ))?;
        let protected = PathBuf::from(checked.trim()).canonicalize()?;
        anyhow::ensure!(
            dir.canonicalize().is_ok_and(|d| same_path(&d, &protected)),
            "--program is only for the protected copy setup makes in Program Files"
        );
        anyhow::ensure!(
            plausible_program(&program.display().to_string()) && program.is_file(),
            "{} is not an installed latch-host.exe",
            program.display()
        );
        let user_sid = for_user.context(
            "setup was not told which user it is for; start it again from Latch's window",
        )?;
        approvable(
            user_sid,
            program,
            &latch_engine::windows::users_running(program),
        )?;
        let wake = crate::wake::probe();
        let plan = Plan {
            elevated_input: elevated_input
                .unwrap_or_else(|| crate::config::HostConfig::load_read_only().elevated_input),
            exe: program,
            adapter: &wake.adapter,
            adapter_description: &wake.description,
        };
        // PowerShell 5.1 reads a BOM-less file as the system ANSI code page, so
        // a Korean/Japanese username or adapter name ("이더넷") would be mangled
        // and the firewall rule would point at a path that does not exist.
        // We write to a file rather than piping to stdin or -EncodedCommand to preserve
        // reliable UTF-8 BOM detection and avoid Windows command-line length limits.
        // The folder is administrators' only; create_new with a random name
        // still never opens a file that is already there.
        use std::io::Write;
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(script(&plan, user_sid).as_bytes());
        let mut tmp = None;
        for _ in 0..5 {
            let candidate = dir.join(format!(
                "latch-setup-{}-{:032x}.ps1",
                std::process::id(),
                rand::random::<u128>()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)?;
                    tmp = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("create the setup script"),
            }
        }
        let tmp = tmp.context("could not allocate unique setup script path")?;
        let log = dir.join("setup.log");
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let status = powershell_command()
            .args([
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                tmp.to_str().unwrap_or(""),
            ])
            .stdout(out)
            .stderr(err)
            .status();
        let _ = std::fs::remove_file(&tmp);
        let status = status.context("run setup script")?;
        anyhow::ensure!(status.success(), "setup failed (see {})", log.display());
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (program, for_user, elevated_input);
        anyhow::bail!("setup runs on Windows only")
    }
}

/// Whether the elevated step may approve `sid`, as the unelevated window
/// passed it across the prompt: a person's account (never SYSTEM, a
/// service or a group) that runs `program`, the Latch that asked, right
/// now (`running`: the users running it).
#[cfg(windows)]
fn approvable(sid: &str, program: &Path, running: &[String]) -> Result<()> {
    anyhow::ensure!(
        latch_engine::windows::is_user_sid(sid),
        "setup was asked for {sid:?}, which is not a user's account"
    );
    anyhow::ensure!(
        running.iter().any(|u| u == sid),
        "setup was asked for {sid}, who is not running {}",
        program.display()
    );
    Ok(())
}

/// Whether two canonical paths name the same folder: Windows compares
/// names without regard to case.
#[cfg(windows)]
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// What the staging step may pass as `--program`: a full local path to an
/// `.exe`. It is only ever named in firewall rules, never run or written.
#[cfg(any(windows, test))]
fn plausible_program(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() > 7
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && path.to_ascii_lowercase().ends_with(".exe")
        && !path.contains(['"', '\n', '\r'])
}

/// Undo what setup and the installers registered, for this user and (on
/// Windows, through one administrator prompt) for the PC: the service and
/// its login entry, the firewall rules, tasks, the power settings setup
/// changed, and on a Mac the privacy grants. The program, its settings and
/// its logs stay. Safe to run again. Returns one line per thing done or
/// left; an error carries the same lines and what did not happen.
pub fn uninstall() -> Result<Vec<String>> {
    #[cfg(windows)]
    {
        let user = powershell_command()
            .arg("-EncodedCommand")
            .arg(encoded_command(&uninstall_user_ps(std::process::id())))
            .output()
            .context("run the uninstall script")?;
        let mut done: Vec<String> = String::from_utf8_lossy(&user.stdout)
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let status = powershell_command()
            .args(["-Command", &launch_script(&uninstall_machine_ps())])
            .status()
            .context("launch the administrator step")?;
        match status.code() {
            Some(0) => done.push(
                "Removed the firewall rules and tasks, put back the power settings setup changed, and removed setup's folder in Program Files.".into(),
            ),
            Some(exit::DECLINED) => {
                done.push("The administrator prompt was declined: the firewall rules and power settings are unchanged. Run latch-host --uninstall again and choose Yes.".into());
                anyhow::bail!(done.join("\n"));
            }
            _ => {
                done.push("The administrator step did not finish. Run latch-host --uninstall again.".into());
                anyhow::bail!(done.join("\n"));
            }
        }
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            done.push(format!(
                "Left in place: the program, your settings and its logs in {}. Delete that folder to remove them.",
                dir.display()
            ));
        }
        Ok(done)
    }
    #[cfg(not(windows))]
    crate::unix_setup::uninstall()
}

/// Say how a command-line run went: on standard output, and on Windows
/// also in a message box, since `latch-host.exe` has no console and the
/// Apps page in Settings starts `--uninstall` with no window to print in.
/// Not in session 0 (SSH), where nobody would see the box or close it.
pub fn tell(title: &str, text: &str, failed: bool) {
    #[cfg(not(windows))]
    let _ = title;
    if failed {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
        };
        let mut session = 0;
        // SAFETY: a valid out pointer for the session id.
        let interactive = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.is_ok()
            && session != 0;
        if interactive {
            let icon = if failed {
                MB_ICONWARNING
            } else {
                MB_ICONINFORMATION
            };
            // SAFETY: both strings outlive the call; no owner window.
            unsafe {
                MessageBoxW(
                    HWND::default(),
                    &HSTRING::from(text),
                    &HSTRING::from(title),
                    MB_OK | icon,
                );
            }
        }
    }
}

/// The signed-in user's part of `--uninstall`: stop Latch (but not this
/// process, `own_pid`), remove its Run values, the task an install over SSH
/// starts it with, the shortcuts and the Apps entry `install-host.ps1`
/// writes.
#[cfg(any(windows, test))]
fn uninstall_user_ps(own_pid: u32) -> String {
    format!(
        r#"$ErrorActionPreference = 'Continue'
try {{ Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:{port}/v1/quit' -ContentType 'application/json' -Body '{{}}' -TimeoutSec 2 | Out-Null; Start-Sleep -Milliseconds 800 }} catch {{}}
$session = (Get-Process -Id $PID).SessionId
Get-Process latch-host, brolink-host -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -ne {own_pid} -and $_.SessionId -eq $session }} | Stop-Process -Force -ErrorAction SilentlyContinue
Write-Output "Stopped Latch."
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
foreach ($name in '{run}', '{legacy_run}') {{ Remove-ItemProperty -LiteralPath $run -Name $name -ErrorAction SilentlyContinue }}
if (Get-ScheduledTask -TaskName '{task}' -ErrorAction SilentlyContinue) {{
    Unregister-ScheduledTask -TaskName '{task}' -Confirm:$false -ErrorAction SilentlyContinue
}}
Write-Output "Latch no longer starts when you sign in."
foreach ($folder in @([Environment]::GetFolderPath('Desktop'), (Join-Path ([Environment]::GetFolderPath('StartMenu')) 'Programs'))) {{
    foreach ($name in 'Latch Host.lnk', 'BroLink Host.lnk') {{ Remove-Item -LiteralPath (Join-Path $folder $name) -Force -ErrorAction SilentlyContinue }}
}}
Remove-Item -LiteralPath '{apps}' -Recurse -Force -ErrorAction SilentlyContinue
Write-Output "Removed the shortcuts and the entry in Settings > Apps."
"#,
        port = CONTROL_PORT,
        run = "LatchHost",
        legacy_run = crate::legacy::WINDOWS_RUN_VALUE,
        task = START_TASK,
        apps = APPS_KEY,
    )
}

/// The task `install-host.ps1` starts the service with over SSH.
#[cfg(any(windows, test))]
const START_TASK: &str = "LatchHostStart";
/// The Settings > Apps entry `install-host.ps1` writes for this user.
#[cfg(any(windows, test))]
const APPS_KEY: &str = r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Latch";

/// The administrator's part of `--uninstall`, a fixed script that names no
/// file of the user's: the firewall rules (and every older name, through
/// the same retire step setup runs), the start task, the power settings
/// recorded under [`SAVED_KEY`] put back, and setup's folder in Program
/// Files. The network card's wake settings and the virtual display's sizes
/// stay: they change nothing until something asks for them.
#[cfg(any(windows, test))]
fn uninstall_machine_ps() -> String {
    format!(
        r#"# Latch uninstall, the administrator's part. Generated by latch-host.
$ErrorActionPreference = 'Continue'
function Step($m) {{ Write-Output "[$(Get-Date -Format HH:mm:ss)] $m" }}
$setupFailed = $false
{protection}
Step "Latch uninstall started"
{retire}
Step "Removing Latch's firewall rules and start task"
foreach ($rule in 'Latch Host', 'Latch Media', 'Latch wake') {{ netsh advfirewall firewall delete rule name="$rule" | Out-Null }}
if (Get-ScheduledTask -TaskName '{task}' -ErrorAction SilentlyContinue) {{
    Unregister-ScheduledTask -TaskName '{task}' -Confirm:$false -ErrorAction SilentlyContinue
}}
Stop-ScheduledTask -TaskName '{input_task}' -ErrorAction SilentlyContinue
if (Get-ScheduledTask -TaskName '{input_task}' -ErrorAction SilentlyContinue) {{
    Unregister-ScheduledTask -TaskName '{input_task}' -Confirm:$false -ErrorAction SilentlyContinue
}}
{sas}
$latchSaved = '{key}'
Step "Removing the ViGEmBus gamepad driver if Latch's setup installed it"
if ((Get-ItemProperty -LiteralPath $latchSaved -Name '{mark}' -ErrorAction SilentlyContinue).'{mark}' -eq '1') {{
    $vigem = {entry}
    $removed = $true
    if ($vigem -and $vigem.PSChildName -match '^\{{[0-9A-Fa-f-]{{36}}\}}$') {{
        $p = Start-Process -FilePath msiexec.exe -ArgumentList '/x', $vigem.PSChildName, '/qn', '/norestart', 'REBOOT=ReallySuppress' -Wait -PassThru
        if ($p.ExitCode -notin 0, 1605, 3010) {{ Write-Output "  msiexec exited with $($p.ExitCode)"; $removed = $false }}
    }} elseif ($vigem) {{ Write-Output "  no product code in '$($vigem.PSChildName)'"; $removed = $false }}
    if ($removed) {{ Remove-ItemProperty -LiteralPath $latchSaved -Name '{mark}' -ErrorAction SilentlyContinue }} else {{ $setupFailed = $true }}
}} else {{ Write-Output "  not installed by Latch; left alone" }}
Step "Putting back the power settings setup changed"
$saved = Get-ItemProperty -LiteralPath $latchSaved -ErrorAction SilentlyContinue
if ($saved) {{
    $restored = $true
    $scheme = $saved.PowerScheme
    if ($scheme -notmatch '^[0-9a-fA-F]{{8}}(-[0-9a-fA-F]{{4}}){{3}}-[0-9a-fA-F]{{12}}$') {{ $scheme = 'SCHEME_CURRENT' }} # older records
    foreach ($s in @(@('StandbyTimeoutAC', 'STANDBYIDLE'), @('HibernateTimeoutAC', 'HIBERNATEIDLE'))) {{
        $v = $saved.($s[0])
        if ($v -match '^[0-9]+$') {{
            powercfg /setacvalueindex $scheme SUB_SLEEP $s[1] $v
            if ($LASTEXITCODE -ne 0) {{ $restored = $false }}
        }}
    }}
    powercfg /setactive SCHEME_CURRENT
    if ($LASTEXITCODE -ne 0) {{ $restored = $false }}
    if ($saved.HiberbootEnabled -eq 'absent') {{
        Remove-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -ErrorAction SilentlyContinue
    }}
    if ($saved.HiberbootEnabled -match '^[0-9]+$') {{
        try {{ Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value ([int]$saved.HiberbootEnabled) -Type DWord -ErrorAction Stop }} catch {{ Write-Output "  Fast Startup: $_"; $restored = $false }}
    }}
    # Kept until everything is back, so running this again finishes the job.
    if ($restored) {{ Remove-Item -LiteralPath $latchSaved -Recurse -Force }} else {{ $setupFailed = $true }}
}} else {{ Write-Output "  nothing recorded; the power settings stay as they are" }}
Step "Removing setup's folder and the input helper"
$latchDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Latch'
$setupDir = Join-Path $latchDir 'Setup'
if (Test-Path -LiteralPath $setupDir) {{
    try {{ Assert-ProtectedTree $setupDir; Remove-Item -LiteralPath $setupDir -Recurse -Force -ErrorAction Stop }} catch {{ Write-Output "  setup folder: $_"; $setupFailed = $true }}
}}
$inputDir = Join-Path $latchDir 'Input'
if (Test-Path -LiteralPath $inputDir) {{
    $inputExe = Join-Path $inputDir 'latch-host.exe'
    Get-Process -Name 'latch-host' -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -ieq $inputExe }} | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    try {{ Assert-ProtectedTree $inputDir; Remove-Item -LiteralPath $inputDir -Recurse -Force -ErrorAction Stop }} catch {{ Write-Output "  input helper folder: $_"; $setupFailed = $true }}
}}
{vdd}
foreach ($empty in @($latchDir, 'HKLM:\SOFTWARE\Latch')) {{
    if ((Test-Path -LiteralPath $empty) -and -not (Get-ChildItem -LiteralPath $empty -Force)) {{ Remove-Item -LiteralPath $empty -Force }}
}}
if ($setupFailed) {{ Step "Some steps failed. See the lines above."; exit 1 }}
Step "Latch uninstall finished"
exit 0
"#,
        protection = protection_checks_ps(),
        retire = crate::legacy::retire_windows_ps(),
        vdd = crate::virtual_display::uninstall_ps(),
        task = START_TASK,
        input_task = INPUT_TASK,
        sas = sas_policy_removal_ps(),
        key = SAVED_KEY,
        mark = VIGEM_MARK,
        entry = VIGEM_ENTRY,
    )
}

/// Register or remove `latch-host.exe --background` under the current
/// user's Run key.
pub fn set_start_with_windows(enable: bool, exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let reg = |args: &[&str]| {
            std::process::Command::new("reg")
                .args(args)
                .creation_flags(0x0800_0000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        };
        // An earlier version's value goes either way: on, it is replaced;
        // off, it would still start the host.
        let _ = reg(&["delete", KEY, "/v", crate::legacy::WINDOWS_RUN_VALUE, "/f"]);
        let status = if enable {
            reg(&[
                "add",
                KEY,
                "/v",
                "LatchHost",
                "/t",
                "REG_SZ",
                "/d",
                &run_value(exe),
                "/f",
            ])?
        } else {
            reg(&["delete", KEY, "/v", "LatchHost", "/f"])?
        };
        anyhow::ensure!(
            status.success() || (!enable && !starts_with_windows()),
            "could not update the Run key"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    crate::unix_setup::set_autostart(enable, exe)
}

/// Keep the login registration current from the background service: the
/// Run value on Windows, the launch agent or user unit elsewhere. Written
/// only; nothing is started or stopped, because the caller is the service.
pub fn register_autostart(exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        set_start_with_windows(true, exe)
    }
    #[cfg(not(windows))]
    crate::unix_setup::register_autostart(exe)
}

pub fn starts_with_windows() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        ["LatchHost", crate::legacy::WINDOWS_RUN_VALUE]
            .iter()
            .any(|value| {
                std::process::Command::new("reg")
                    .args([
                        "query",
                        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                        "/v",
                        value,
                    ])
                    .creation_flags(0x0800_0000)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            })
    }
    #[cfg(not(windows))]
    crate::unix_setup::autostart_enabled()
}

#[cfg(any(windows, test))]
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\" --background", exe.display())
}

#[cfg(test)]
const SPECIAL_PATH: &str = r"C:\Users\’ ‘ ‚ ‛ ' $ ` lab\Latch\latch-host.exe";

#[cfg(test)]
mod tests {
    use super::*;

    /// The user who asked for setup, unelevated.
    const TEST_SID: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn setup_opens_both_control_transports_and_keeps_wake_and_display_steps() {
        let s = script(
            &Plan {
                elevated_input: true,
                exe: Path::new("C:\\Users\\O'Brien\\Latch\\latch-host.exe"),
                adapter: "Ethernet",
                adapter_description: "Realtek",
            },
            TEST_SID,
        );
        // Tailscale's IPv4 range and its IPv6 one (IN-10).
        assert!(
            s.contains("protocol=TCP localport=47850 remoteip=100.64.0.0/10,fd7a:115c:a1e0::/48")
        );
        assert!(
            s.contains("protocol=UDP localport=47850 remoteip=100.64.0.0/10,fd7a:115c:a1e0::/48")
        );
        assert!(s.contains(&ps_data(r"C:\Users\O'Brien\Latch\latch-host.exe")));
        assert!(s.contains("HiberbootEnabled"));
        assert!(s.contains("*WakeOnMagicPacket"));
        assert!(s.contains("vdd_settings.xml"));
        // The one download is the pinned gamepad driver (no Sunshine).
        assert_eq!(s.matches("Invoke-WebRequest").count(), 1);
        assert!(s.contains(&format!("-Uri '{VIGEM_URL}'")));
        assert!(!s.contains("--creds"));
    }

    /// Setup installs ViGEmBus only when nothing has, runs only the file
    /// whose hash it pins, never restarts Windows, and marks what it
    /// installed; uninstall removes only that.
    #[test]
    fn setup_installs_the_pinned_gamepad_driver_and_uninstall_only_removes_its_own() {
        let s = script(
            &Plan {
                elevated_input: true,
                exe: Path::new(r"C:\Latch\latch-host.exe"),
                adapter: "",
                adapter_description: "",
            },
            TEST_SID,
        );
        let step = s
            .split("Step \"Installing the ViGEmBus gamepad driver")
            .nth(1)
            .expect("a gamepad driver step");
        let at = |needle: &str| step.find(needle).unwrap_or_else(|| panic!("no {needle}"));
        assert!(at("already installed") < at("Invoke-WebRequest"));
        assert!(at("Assert-ProtectedTree $vigemDir") < at("Invoke-WebRequest"));
        assert!(at("Invoke-WebRequest") < at(VIGEM_SHA256));
        assert!(at(VIGEM_SHA256) < at("Start-Process"));
        assert!(step.contains("'/qn', '/norestart', 'REBOOT=ReallySuppress'"));
        assert!(!s.contains("Restart-Computer") && !s.contains("shutdown"));
        assert!(at("Start-Process") < at(VIGEM_MARK));
        // New-Item -Force would empty the key the power settings are in.
        assert!(!step.contains("New-Item -Path $latchSaved -Force"));
        // The mark goes under the saved key after the power settings were
        // recorded, so its existence never stops them being recorded.
        assert!(
            s.find("Recording the power settings").unwrap()
                < s.find("Installing the ViGEmBus").unwrap()
        );
        let m = uninstall_machine_ps();
        let removal = m
            .split("Step \"Removing the ViGEmBus gamepad driver")
            .nth(1)
            .unwrap();
        assert!(removal.find(VIGEM_MARK).unwrap() < removal.find("msiexec.exe").unwrap());
        assert!(removal
            .contains("'/x', $vigem.PSChildName, '/qn', '/norestart', 'REBOOT=ReallySuppress'"));
        assert!(removal.contains("not installed by Latch; left alone"));
        // Read before the power restore deletes the saved key.
        assert!(
            m.find("Removing the ViGEmBus").unwrap() < m.find("Putting back the power").unwrap()
        );
    }

    /// The power settings setup changes are recorded first, once, so that
    /// `--uninstall` can put them back (IN-11).
    #[test]
    fn setup_records_the_power_settings_before_changing_them() {
        let s = script(
            &Plan {
                elevated_input: true,
                exe: Path::new("C:\\Latch\\latch-host.exe"),
                adapter: "",
                adapter_description: "",
            },
            TEST_SID,
        );
        let record = s
            .find(r"$latchSaved = 'HKLM:\SOFTWARE\Latch\Setup'")
            .unwrap();
        let only_once = s
            .find("if (-not (Test-Path -LiteralPath $latchSaved))")
            .unwrap();
        for change in [
            "-Name HiberbootEnabled -Value 0",
            "standby-timeout-ac 0",
            "hibernate-timeout-ac 0",
        ] {
            assert!(record < s.find(change).unwrap(), "{change}");
        }
        assert!(record < only_once);
        for name in ["HiberbootEnabled", "StandbyTimeoutAC", "HibernateTimeoutAC"] {
            assert!(
                s.contains(&format!("-Name {name}")) || s.contains(&format!("'{name}'")),
                "{name}"
            );
        }
        assert!(s.contains("powercfg /query SCHEME_CURRENT SUB_SLEEP $s[1]"));
    }

    /// `--uninstall` reverses setup: firewall rules, tasks, the recorded
    /// power settings, setup's folder, and for the user the Run values,
    /// shortcuts and Apps entry. The administrator's part names no file of
    /// the user's (IN-07, IN-11).
    #[test]
    fn uninstall_reverses_what_setup_and_the_installer_registered() {
        let m = uninstall_machine_ps();
        for rule in ["'Latch Host'", "'Latch Media'", "'Latch wake'"] {
            assert!(m.contains(rule), "{rule}");
        }
        assert!(m.contains("netsh advfirewall firewall delete rule name=\"$rule\""));
        assert!(m.contains("Unregister-ScheduledTask -TaskName 'LatchHostStart'"));
        assert!(m.contains("'LatchEngineUser'"), "older names go too");
        assert!(m.contains(r"$latchSaved = 'HKLM:\SOFTWARE\Latch\Setup'"));
        for put_back in [
            "powercfg /setacvalueindex $scheme SUB_SLEEP $s[1] $v",
            "powercfg /setactive SCHEME_CURRENT",
            "-Name HiberbootEnabled -Value ([int]$saved.HiberbootEnabled)",
        ] {
            let at = m.find(put_back).unwrap_or_else(|| panic!("{put_back}"));
            assert!(at < m.find("Remove-Item -LiteralPath $latchSaved").unwrap());
        }
        // Setup records the very names uninstall reads back.
        let setup = script(
            &Plan {
                elevated_input: true,
                exe: Path::new("C:\\Latch\\latch-host.exe"),
                adapter: "",
                adapter_description: "",
            },
            TEST_SID,
        );
        for (name, setting) in [
            ("StandbyTimeoutAC", "STANDBYIDLE"),
            ("HibernateTimeoutAC", "HIBERNATEIDLE"),
        ] {
            let pair = format!("@('{name}', '{setting}')");
            assert!(setup.contains(&pair) && m.contains(&pair), "{pair}");
        }
        assert!(m.contains("Join-Path $latchDir 'Setup'"));
        // A virtual display setup pointed at Latch's protected copy is
        // pointed back, and the copy goes, before the empty folders do.
        let vdd = m.find("Pointing the virtual display back").unwrap();
        assert!(vdd > m.find("$latchDir = ").unwrap());
        assert!(vdd < m.find("foreach ($empty in").unwrap());
        assert!(m.contains("-Name VDDPATH -Value $vddPrevious"));
        assert!(m.contains("Remove-Item -LiteralPath $vddOurs -Recurse"));
        let s = crate::virtual_display::setup_ps();
        assert!(s.contains("-Name 'PreviousVddPath' -Value $vddPrevious"));
        assert!(!m.contains("LOCALAPPDATA") && !m.contains("HKCU"));
        assert!(m.contains("exit 1") && m.ends_with("exit 0\n"));

        let u = uninstall_user_ps(4242);
        assert!(u.contains("$_.Id -ne 4242"), "never stops itself");
        assert!(u.contains("/v1/quit"));
        assert!(u.contains("'LatchHost', 'BroLinkHost'"));
        assert!(u.contains(r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"));
        assert!(u.contains("'Latch Host.lnk'"));
        assert!(u.contains(r"'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Latch'"));
        assert!(u.contains("Unregister-ScheduledTask -TaskName 'LatchHostStart'"));
    }

    /// Setup puts the input helper in a folder only administrators can
    /// change, from the checked copy it runs from, and registers the task
    /// that runs it elevated; uninstall takes both away again.
    #[test]
    fn setup_installs_the_elevated_input_helper_and_uninstall_removes_it() {
        let setup = script(
            &Plan {
                elevated_input: true,
                exe: Path::new(r"C:\Latch\latch-host.exe"),
                adapter: "",
                adapter_description: "",
            },
            TEST_SID,
        );
        let step = input_helper_ps(true, TEST_SID);
        assert!(setup.contains(&step));
        assert!(step.contains(r"'Latch\Setup\latch-host.exe'"));
        assert!(step.contains(r"'Latch\Input'"));
        let copy = step.find("Copy-Item -LiteralPath $inputSource").unwrap();
        for check in [
            "Assert-ProtectedTree $inputSource",
            "Stop-ScheduledTask -TaskName 'LatchInput'",
            "Stop-Process -Force",
            // Made again when anything in it is not administrators'.
            "Reset-ProtectedFolder $inputDir",
        ] {
            assert!(step.find(check).unwrap() < copy, "{check} before the copy");
        }
        // What setup wrote is checked, repaired if the owner is not trusted,
        // and the whole folder checked again before the task is registered.
        let register = step.find("Register-ScheduledTask").unwrap();
        let repair = step
            .find("foreach ($written in $inputExe, $hostRecord, $userRecord) { if (Test-Protected $written) { Repair-Protected $written } }")
            .expect("repairs what it wrote");
        let verify = step
            .find("if (-not (Test-ProtectedContents $inputDir)) { throw")
            .expect("checks the folder again");
        assert!(copy < repair && repair < verify && verify < register);
        assert!(step.contains("-Argument '--input-helper'"));
        // LocalSystem, by SID so no translated account name can miss.
        assert!(step.contains(
            "New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest"
        ));
        assert!(!step.contains("-LogonType Interactive"));
        assert!(step.contains("-MultipleInstances IgnoreNew"));
        assert!(step.contains("Register-ScheduledTask -TaskName 'LatchInput'"));
        // The approved user is the one who asked for setup, recorded beside
        // the helper, and checked to be a real account before anything.
        assert!(step.contains(&format!("$approvedSid = {}", ps_data(TEST_SID))));
        let translate = step
            .find("([Security.Principal.SecurityIdentifier]$approvedSid).Translate([Security.Principal.NTAccount])")
            .expect("the SID names an account");
        assert!(translate < copy);
        assert!(step.contains("Set-Content -LiteralPath $userRecord -Value $approvedSid"));
        assert!(step.contains("'user-sid.txt'"));
        assert!(
            !step.contains("$identity"),
            "never the elevated administrator"
        );
        // Only that user may start the SYSTEM task, besides SYSTEM and
        // administrators; nobody else may change it.
        let secure = step
            .find(".GetTask('LatchInput').SetSecurityDescriptor(\"D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;$approvedSid)\", 0)")
            .expect("task security");
        assert!(register < secure);
        for broad in [";AU)", ";WD)", ";IU)", ";BU)"] {
            assert!(!step.contains(broad), "{broad}");
        }
        assert!(step.contains("$setupFailed = $true"));
        assert!(
            !step.contains("LOCALAPPDATA"),
            "nothing elevated from the user's folder"
        );
        // Ctrl+Alt+Delete: the policy is set only when unset, the mark is
        // written first, and taken back when the write fails.
        let unset = step
            .find("if ($null -eq (Get-ItemProperty -LiteralPath 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System' -Name SoftwareSASGeneration")
            .expect("checks the policy first");
        let mark = step
            .find(
                r"New-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Latch\Input' -Name SasPolicyAdded",
            )
            .unwrap();
        let write = step
            .find("-Name SoftwareSASGeneration -Value 1 -PropertyType DWord")
            .unwrap();
        assert!(unset < mark && mark < write, "intent first");
        let rollback = step
            .find(r"Remove-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Latch\Input' -Name SasPolicyAdded")
            .expect("rolls the mark back");
        assert!(write < rollback);

        let disabled = input_helper_ps(false, TEST_SID);
        assert!(disabled.contains("Stop-ScheduledTask"));
        assert!(disabled.contains("Unregister-ScheduledTask"));
        assert!(!disabled.contains("-RunLevel Highest"));
        // Turning it off stops the agents, which run apart from the task,
        // and removes the copy.
        let stop = disabled
            .find("Where-Object { $_.Path -ieq $inputExe } | Stop-Process -Force")
            .expect("stops the agents");
        assert!(stop < disabled.find("Remove-Tree $inputDir").unwrap());
        // Takes the policy back only when setup set it.
        let mark = disabled.find("SasPolicyAdded -eq '1'").unwrap();
        assert!(mark < disabled.find("Remove-ItemProperty").unwrap());
        assert!(step.contains("host-path.txt"));
        let m = uninstall_machine_ps();
        assert!(m.contains(&sas_policy_removal_ps()));
        assert!(m.contains("Unregister-ScheduledTask -TaskName 'LatchInput'"));
        assert!(m.contains("$inputDir = Join-Path $latchDir 'Input'"));
        assert!(
            m.find("Assert-ProtectedTree $inputDir").unwrap()
                < m.find("Remove-Item -LiteralPath $inputDir").unwrap()
        );
    }

    /// Cleanup removes SoftwareSASGeneration only while it is exactly the
    /// DWORD 1 setup wrote; an administrator's later value stays.
    #[test]
    fn sas_cleanup_removes_only_the_value_setup_wrote() {
        let removal = sas_policy_removal_ps();
        let marked = removal.find("SasPolicyAdded -eq '1'").unwrap();
        let exact = removal
            .find("GetValueKind('SoftwareSASGeneration') -eq [Microsoft.Win32.RegistryValueKind]::DWord -and $sasKey.GetValue('SoftwareSASGeneration') -eq 1")
            .expect("checks the value is still setup's");
        let remove = removal
            .find("Remove-ItemProperty -LiteralPath 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System' -Name SoftwareSASGeneration")
            .unwrap();
        assert!(marked < exact && exact < remove);
        // The mark goes either way, so a later setup starts afresh.
        let unmark = removal
            .find(r"Remove-Item -LiteralPath 'HKLM:\SOFTWARE\Latch\Input'")
            .unwrap();
        assert!(remove < unmark);
        assert_eq!(
            removal[..unmark].matches("Remove-ItemProperty").count(),
            1,
            "{removal}"
        );
    }

    /// The SID crosses the prompt on a command line: the elevated step
    /// takes only a person's account that runs the Latch that asked.
    #[cfg(windows)]
    #[test]
    fn the_elevated_step_approves_only_the_user_running_the_asking_latch() {
        let program = Path::new(r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe");
        let running = vec![TEST_SID.to_string()];
        assert!(approvable(TEST_SID, program, &running).is_ok());
        // Another account, even a real one, that does not run it.
        assert!(approvable("S-1-5-21-1-2-3-1002", program, &running).is_err());
        // Not an account at all, even when "running".
        for sid in [
            "S-1-5-18",
            "S-1-5-32-544",
            "S-1-5-21-1-2-3-1001)(A;;GA;;;WD",
        ] {
            assert!(
                approvable(sid, program, &[sid.to_string()]).is_err(),
                "{sid}"
            );
        }
        // This test process: its own user runs it.
        let me = latch_engine::windows::current_user_sid().unwrap();
        let exe = std::env::current_exe().unwrap();
        assert!(latch_engine::windows::users_running(&exe).contains(&me));
    }

    #[cfg(windows)]
    #[test]
    fn the_engine_starts_the_task_setup_registers() {
        assert_eq!(INPUT_TASK, latch_engine::windows::INPUT_TASK);
        let exe = latch_engine::windows::input_helper_exe().unwrap();
        assert!(exe.ends_with(r"Latch\Input\latch-host.exe"));
    }

    /// Every PowerShell and schtasks setup starts is Windows' own, by full
    /// path, without profiles, finding modules and programs only in the
    /// system folder and starting there: nothing the user can write is
    /// loaded into a process that may be elevated.
    #[test]
    fn setup_starts_windows_tools_in_a_trusted_environment() {
        let system = system_dir();
        for (command, program) in [
            (powershell_command(), powershell_exe()),
            (schtasks_command(), system.join("schtasks.exe")),
        ] {
            assert_eq!(Path::new(command.get_program()), program);
            assert!(program.starts_with(&system));
            assert_eq!(command.get_current_dir(), Some(system.as_path()));
            let envs: std::collections::HashMap<_, _> = command
                .get_envs()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(PathBuf::from)))
                .collect();
            assert_eq!(envs.get("PATH"), Some(&Some(system.clone())));
            assert_eq!(
                envs.get("PSModulePath"),
                Some(&Some(system.join(r"WindowsPowerShell\v1.0\Modules")))
            );
        }
        let args: Vec<_> = powershell_command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-NoProfile", "-NonInteractive"]);
        // No other way to start them is left in setup.
        let source = include_str!("setup.rs");
        let spawn = ["std::process::Command::new(", "powershell_exe())"].concat();
        assert!(!source.contains(&spawn));
        assert_eq!(
            source
                .matches(&["Command::new(", "program)"].concat())
                .count(),
            1
        );
    }

    /// What PowerShell itself reports in that environment: no user module
    /// folder, and the system folder as its location.
    #[cfg(windows)]
    #[test]
    fn trusted_powershell_sees_only_system_modules() {
        let out = powershell(
            "Write-Output $env:PSModulePath; Write-Output $env:PATH; Write-Output (Get-Location).Path",
        )
        .unwrap();
        let lines: Vec<_> = out.lines().map(str::trim).collect();
        let system = system_dir().display().to_string();
        let modules = system_dir()
            .join(r"WindowsPowerShell\v1.0\Modules")
            .display()
            .to_string();
        let documents = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users".into());
        assert!(
            lines[0].split(';').all(|dir| !dir
                .to_ascii_lowercase()
                .starts_with(&documents.to_ascii_lowercase())),
            "{out}"
        );
        assert!(
            lines[0]
                .split(';')
                .any(|dir| dir.eq_ignore_ascii_case(&modules)),
            "{out}"
        );
        assert!(lines[1].eq_ignore_ascii_case(&system), "{out}");
        assert!(lines[2].eq_ignore_ascii_case(&system), "{out}");
        assert!(!task_registered("LatchNoSuchTask-7f3a"));
    }

    #[test]
    fn an_elevated_caller_runs_the_step_without_a_prompt() {
        let launch = launch_script("exit 0");
        let direct = launch
            .find("IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)")
            .unwrap();
        assert!(direct < launch.find("-Verb RunAs").unwrap());
        assert!(launch.contains("; exit $LASTEXITCODE }"), "{launch}");
    }

    #[test]
    fn incomplete_power_backups_cannot_change_settings() {
        let record = record_power_ps();
        assert!(record.contains("throw 'Could not read the active power plan'"));
        assert!(record.contains("throw 'Could not read the power timeout'"));
        assert!(record.contains("PowerScheme = $scheme"));
        assert!(uninstall_machine_ps().contains("$scheme = $saved.PowerScheme"));
        assert!(uninstall_machine_ps().contains("$saved.HiberbootEnabled -eq 'absent'"));
    }

    #[test]
    fn elevated_commands_fit_the_windows_command_line() {
        for script in [
            stage_script(r"C:\Latch\latch-host.exe", &"ab".repeat(32), TEST_SID, true),
            uninstall_machine_ps(),
        ] {
            assert!(launch_script(&script).encode_utf16().count() < 32000);
        }
    }

    #[test]
    fn setup_during_a_stream_needs_a_second_click() {
        let now = std::time::Instant::now();
        assert!(refuse_during_stream(true, None, now));
        assert!(!refuse_during_stream(true, Some(now), now));
        assert!(refuse_during_stream(
            true,
            Some(now),
            now + SETUP_CONFIRM_WINDOW
        ));
        assert!(!refuse_during_stream(false, None, now));
    }

    #[test]
    fn the_run_key_quotes_the_path() {
        assert!(run_value(Path::new("C:/a b/latch-host.exe")).starts_with('\"'));
    }

    /// The UAC prompt runs Windows' own PowerShell, which copies Latch into
    /// Program Files, checks the copy is the pinned file and runs the copy:
    /// nothing elevated loads or runs from the user's folder, and no folder
    /// the caller names is written to (IN-01, IN-02).
    #[test]
    fn the_elevated_step_runs_a_checked_copy_from_program_files() {
        let program = SPECIAL_PATH;
        let stage = stage_script(program, &"ab".repeat(32), TEST_SID, true);
        assert!(stage.contains(&format!("$program = {}", ps_data(program))));
        assert!(!stage.contains(program));
        assert!(stage.contains(r"[Environment]::GetFolderPath('ProgramFiles')) 'Latch\Setup'"));
        let copy = stage.find("[IO.File]::Copy($program, $copy)").unwrap();
        let check = stage
            .find("Get-FileHash -LiteralPath $copy -Algorithm SHA256")
            .unwrap();
        let run = stage.find("ProcessStartInfo $copy").unwrap();
        assert!(
            copy < check && check < run,
            "copy, then check, then run the copy"
        );
        assert!(stage.contains(&format!("$sha256 = {}", ps_data(&"ab".repeat(32)))));
        assert!(stage.contains(".Hash -ne $sha256"));
        assert!(stage.contains("'--setup-elevated --program \"' + $program + '\" --for-user "));
        assert!(!stage.contains("--local-app-data"));
        assert!(!stage.contains("ProcessStartInfo $program"));

        let launch = launch_script(&stage);
        assert!(launch.contains(&format!(
            "$ps = {}",
            ps_data(&powershell_exe().display().to_string())
        )));
        assert!(launch.contains("-FilePath $ps -Verb RunAs"));
        assert!(
            !launch.contains("latch-host.exe"),
            "the user's file is never elevated: {launch}"
        );
        let encoded = launch
            .split("$encoded = '")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap();
        assert_eq!(
            launch.matches("-EncodedCommand ").count(),
            2,
            "elevated or prompted, the same script"
        );
        assert_eq!(encoded, encoded_command(&stage));
        assert!(stage.contains("$env:PATH = [Environment]::SystemDirectory"));
        assert!(stage.contains("Set-Location -LiteralPath ([Environment]::SystemDirectory)"));
        assert!(launch.contains("catch { exit 1223 }"));
    }

    #[test]
    fn setup_checks_permissions_and_links_before_writing() {
        let s = stage_script(r"C:\Latch\latch-host.exe", &"ab".repeat(32), TEST_SID, true);
        assert!(s.contains(
            "Reset-ProtectedFolder $dir ([Environment]::GetFolderPath('ProgramFiles')) $pins"
        ));
        assert!(s.contains("if (Test-Protected $copy) { Repair-Protected $copy }"));
        assert!(s.contains("ReparsePoint"));
        assert!(s.contains("GetOwner([Security.Principal.SecurityIdentifier])"));
        assert!(s.find("Reset-ProtectedFolder $dir").unwrap() < s.find("[IO.File]::Copy").unwrap());
        assert!(s.contains("$start.WorkingDirectory = $dir"));
        assert!(s.find("$p.WaitForExit()").unwrap() < s.rfind("$handle.Dispose()").unwrap());
        // The asking user and their setting go to the elevated step.
        assert!(s.contains(&format!("$userSid = {}", ps_data(TEST_SID))));
        assert!(s.contains(&format!("$elevatedInput = {}", ps_data("true"))));
        assert!(s.contains(" + $userSid + ' --elevated-input ' + $elevatedInput"));
    }

    #[test]
    fn embedded_protection_keeps_every_function_and_drops_comments() {
        let embedded = protection_ps();
        for function in [
            "function Test-Protected",
            "function Assert-Protected",
            "function Assert-ProtectedTree",
            "function Repair-Protected",
            "function Move-UntrustedTree",
            "function Use-ProtectedFolder",
            "function Test-ProtectedContents",
            "function Remove-Tree",
            "function Reset-ProtectedFolder",
        ] {
            assert!(embedded.contains(function), "{function}");
        }
        assert!(embedded.lines().all(|l| !l.trim_start().starts_with('#')));
        assert!(embedded.contains("throw 'Setup requires an administrator'"));
        // Uninstall only checks, and needs no more.
        let checks = protection_checks_ps();
        assert!(checks.contains("function Assert-ProtectedTree"));
        assert!(!checks.contains("function Repair-Protected"));
        assert!(!checks.contains("Reset-ProtectedFolder"));
        assert!(uninstall_machine_ps().contains(&checks));
        for function in ["Repair-Protected", "Reset-ProtectedFolder", "Remove-Tree"] {
            assert!(!uninstall_machine_ps().contains(function), "{function}");
        }
    }

    /// Only SYSTEM, Administrators and TrustedInstaller may own or change
    /// what setup runs: never the account running setup, whose SID is
    /// also that of its unelevated processes.
    #[test]
    fn only_system_administrators_and_trustedinstaller_are_trusted() {
        let protection = include_str!("../../../scripts/setup-protection.ps1");
        let trusted = protection
            .split("$trusted = @(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap();
        assert_eq!(trusted.matches("'S-1-").count(), 3, "{trusted}");
        for sid in [
            "'S-1-5-18'",
            "'S-1-5-32-544'",
            "'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'",
        ] {
            assert!(trusted.contains(sid), "{sid}");
        }
        assert!(!trusted.contains("$identity"));
        // Generic all and generic write count as write access.
        assert!(protection.contains("0x10000000 -bor 0x40000000"));
        // Repair makes Administrators the owner and resets to what the
        // checked parent passes on, then checks again; never a link.
        let repair = protection
            .split("function Repair-Protected")
            .nth(1)
            .unwrap();
        let repair = repair.split("\nfunction ").next().unwrap();
        let link = repair.find("ReparsePoint").unwrap();
        let own = repair.find("takeown.exe') /A /F $path").unwrap();
        let reset = repair.find("icacls.exe') $path /reset").unwrap();
        let check = repair.find("Assert-Protected $path").unwrap();
        assert!(link < own && own < reset && reset < check);
    }

    #[test]
    fn the_session_zero_start_task_is_removed_even_on_failure() {
        let installer = include_str!("../../../scripts/install-host.ps1");
        assert!(installer.contains("} finally {"));
        assert!(installer.contains("Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false"));
    }

    /// The SYSTEM helper's broker and agents are also latch-host.exe; the
    /// unelevated installer stops only this user's copy at its own path.
    #[test]
    fn the_installer_stops_only_its_own_host() {
        let installer = include_str!("../../../scripts/install-host.ps1");
        assert!(!installer.contains("Get-Process latch-host"));
        assert!(installer.contains("$candidate.ExecutablePath -ine $DestExe"));
        assert!(installer.contains("$owner.Sid -ne $Me"));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded);
        }
        assert_eq!(base64(&[0xFF, 0xFE, 0xFD]), "//79");
    }

    /// A declined prompt says so plainly and points at no log: nothing ran,
    /// and the log on disk is an older run's (IN-09).
    #[test]
    fn a_declined_prompt_is_not_reported_as_a_failed_setup() {
        let declined = elevation_error(Some(1223));
        assert_eq!(declined, ADMIN_DECLINED);
        assert!(!declined.contains("setup.log") && !declined.contains("AppData"));
        assert!(elevation_error(Some(4)).contains("changed"));
        assert!(elevation_error(Some(1)).contains("setup log"));
        assert!(elevation_error(None).contains("setup log"));
    }

    #[test]
    fn only_a_full_path_to_an_exe_is_named_in_the_firewall_rules() {
        assert!(plausible_program(
            r"C:\Users\Ada Lovelace\AppData\Local\Latch\latch-host.exe"
        ));
        assert!(plausible_program(r"D:\Latch\LATCH-HOST.EXE"));
        for bad in [
            "latch-host.exe",
            r"..\latch-host.exe",
            r"\\server\share\latch-host.exe",
            r"C:\Latch\latch-host.exe\",
            r"C:\Latch\setup.ps1",
            "C:\\Latch\\a\"b.exe",
        ] {
            assert!(!plausible_program(bad), "{bad}");
        }
    }

    #[test]
    fn setup_lifts_windows_block_rules_for_this_executable_only() {
        let s = script(
            &Plan {
                elevated_input: true,
                exe: Path::new(SPECIAL_PATH),
                adapter: SPECIAL_PATH,
                adapter_description: SPECIAL_PATH,
            },
            TEST_SID,
        );
        for variable in ["$latchExe", "$wakeAdapter", "$wakeDevice"] {
            assert!(s.contains(&format!("{variable} = {}", ps_data(SPECIAL_PATH))));
        }
        assert!(!s.contains(SPECIAL_PATH));
        assert!(s.contains(r#""program=$latchExe""#));
        assert!(s.contains("-LiteralPath $hostRecord -Value $latchExe"));
        assert!(s.contains("$_.Program -ieq $latchExe"));
        assert!(s.contains("$_.Action -eq 'Block' -and $_.Direction -eq 'Inbound'"));
        let lift = s.find("Remove-NetFirewallRule").unwrap();
        let allow = s.find("add rule name=\"Latch Host\"").unwrap();
        assert!(
            lift < allow,
            "block rules go before the allow rules are added"
        );
    }

    #[test]
    fn the_generated_script_can_be_dumped_for_a_syntax_check() {
        let Ok(out) = std::env::var("LATCH_DUMP_SETUP_SCRIPT") else {
            return;
        };
        let exe =
            std::env::var("LATCH_DUMP_EXE").unwrap_or_else(|_| "C:\\Latch\\latch-host.exe".into());
        let adapter = std::env::var("LATCH_DUMP_ADAPTER").unwrap_or_default();
        let description = std::env::var("LATCH_DUMP_ADAPTER_DESC").unwrap_or_default();
        std::fs::write(
            out,
            script(
                &Plan {
                    elevated_input: true,
                    exe: Path::new(&exe),
                    adapter: &adapter,
                    adapter_description: &description,
                },
                TEST_SID,
            ),
        )
        .unwrap();
        // Both parts of --uninstall, which CI also runs.
        if let Ok(out) = std::env::var("LATCH_DUMP_UNINSTALL_SCRIPT") {
            std::fs::write(format!("{out}.user.ps1"), uninstall_user_ps(0)).unwrap();
            std::fs::write(out, uninstall_machine_ps()).unwrap();
        }
        // The elevated staging step and the one-line launcher around it.
        if let Ok(out) = std::env::var("LATCH_DUMP_STAGE_SCRIPT") {
            let stage = stage_script(&exe, &"0".repeat(64), TEST_SID, true);
            std::fs::write(format!("{out}.launch.ps1"), launch_script(&stage)).unwrap();
            std::fs::write(out, stage).unwrap();
        }
    }
}
