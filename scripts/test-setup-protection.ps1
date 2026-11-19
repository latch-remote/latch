# Isolated Windows regression: reject junctions, user-writable folders and
# log symlinks before any elevated file write. No real install is touched.
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\setup-protection.ps1"
$root = Join-Path $env:RUNNER_TEMP ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)')
    Set-Acl -LiteralPath $root -AclObject $acl
    Assert-Protected $root
    $unsafe = Join-Path $root 'writable'
    New-Item -ItemType Directory -Path $unsafe | Out-Null
    $acl = Get-Acl -LiteralPath $unsafe
    $users = [Security.Principal.SecurityIdentifier]'S-1-5-32-545'
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($users, 'Modify', 'Allow')))
    Set-Acl -LiteralPath $unsafe -AclObject $acl
    function Must-Reject($path) {
        $rejected = $false
        try { Assert-Protected $path } catch { $rejected = $true }
        if (-not $rejected) { throw "Accepted unsafe path: $path" }
    }
    Must-Reject $unsafe
    $victim = Join-Path $root 'victim.txt'
    Set-Content -LiteralPath $victim -Value 'unchanged'
    $link = Join-Path $root 'setup.log'
    New-Item -ItemType SymbolicLink -Path $link -Target $victim | Out-Null
    Must-Reject $link
    if ((Get-Content -LiteralPath $victim) -ne 'unchanged') { throw 'Victim was changed' }
    Remove-Item -LiteralPath $link -Force
    $junction = Join-Path $root 'junction'
    New-Item -ItemType Junction -Path $junction -Target $unsafe | Out-Null
    Must-Reject $junction
    [IO.Directory]::Delete($junction)

    # The account running setup is not trusted as an owner: its SID is also
    # that of its unelevated processes.
    $me = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $mine = Join-Path $root 'mine.txt'
    Set-Content -LiteralPath $mine -Value 'x'
    $acl = Get-Acl -LiteralPath $mine
    $acl.SetOwner($me)
    Set-Acl -LiteralPath $mine -AclObject $acl
    Must-Reject $mine
    # Nor as a writer, nor anyone holding a generic right instead of a
    # specific one.
    $written = Join-Path $root 'written.txt'
    Set-Content -LiteralPath $written -Value 'x'
    $acl = Get-Acl -LiteralPath $written
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($me, 'Write', 'Allow')))
    Set-Acl -LiteralPath $written -AclObject $acl
    Must-Reject $written
    $generic = Join-Path $root 'generic.txt'
    Set-Content -LiteralPath $generic -Value 'x'
    $acl = Get-Acl -LiteralPath $generic
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;GA;;;BU)')
    Set-Acl -LiteralPath $generic -AclObject $acl
    Must-Reject $generic

    # Repair: a folder setup owns, found owned by the user and writable by
    # Users, goes back to administrators and what its parent passes on.
    $repaired = Join-Path $root 'Latch'
    New-Item -ItemType Directory -Path $repaired | Out-Null
    $acl = Get-Acl -LiteralPath $repaired
    $acl.SetOwner($me)
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($users, 'Modify', 'ContainerInherit, ObjectInherit', 'None', 'Allow')))
    Set-Acl -LiteralPath $repaired -AclObject $acl
    Must-Reject $repaired
    Use-ProtectedFolder (Join-Path $repaired 'Input') $root
    Assert-Protected $repaired
    Assert-Protected (Join-Path $repaired 'Input')
    # A folder setup fills is made again when anything in it is not
    # protected: what was planted there is gone, not adopted.
    $inputDir = Join-Path $repaired 'Input'
    $planted = Join-Path $inputDir 'version.dll'
    Set-Content -LiteralPath $planted -Value 'x'
    $acl = Get-Acl -LiteralPath $planted
    $acl.SetOwner($me)
    Set-Acl -LiteralPath $planted -AclObject $acl
    if (Test-ProtectedContents $inputDir) { throw 'Accepted a planted file' }
    # A junction inside it is removed, never followed.
    New-Item -ItemType Junction -Path (Join-Path $inputDir 'out') -Target $unsafe | Out-Null
    Set-Content -LiteralPath (Join-Path $unsafe 'keep.txt') -Value 'kept'
    Reset-ProtectedFolder $inputDir $root
    if (Test-Path -LiteralPath $planted) { throw 'Kept a planted file' }
    if (Get-ChildItem -LiteralPath $inputDir -Force) { throw 'The folder was not made again' }
    if ((Get-Content -LiteralPath (Join-Path $unsafe 'keep.txt')) -ne 'kept') { throw 'Followed a junction' }
    if (-not (Test-ProtectedContents $inputDir)) { throw 'The new folder is not protected' }
    # A protected folder is left as it is.
    Set-Content -LiteralPath (Join-Path $inputDir 'host-path.txt') -Value 'x'
    Reset-ProtectedFolder $inputDir $root
    if (-not (Test-Path -LiteralPath (Join-Path $inputDir 'host-path.txt'))) { throw 'Removed a protected folder' }
    # Nothing outside the base is touched.
    $refused = $false
    try { Use-ProtectedFolder $env:RUNNER_TEMP $root } catch { $refused = $true }
    if (-not $refused) { throw 'Worked outside the base folder' }
    Write-Output 'setup protection: all checks passed'
} finally {
    Get-ChildItem -LiteralPath $root -Recurse -Force -Attributes ReparsePoint -ErrorAction SilentlyContinue | ForEach-Object { [IO.Directory]::Delete($_.FullName) }
    Remove-Item -LiteralPath $root -Recurse -Force
}
