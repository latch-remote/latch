# Isolated Windows regression: reject junctions, user-writable folders and
# log symlinks before any elevated file write. No real install is touched.
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\setup-protection.ps1"
# On the system drive, as %ProgramFiles% is: setup checks every folder up
# from the volume root, and a CI runner's work drive lets users write there.
$root = Join-Path ([IO.Path]::GetPathRoot([Environment]::SystemDirectory)) ('LatchProtectionTest-' + [guid]::NewGuid().ToString('N'))
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
    # The base can be protected while its writable parent can redirect it.
    $badBase = Join-Path $unsafe 'Program Files'
    $protected = New-Object Security.AccessControl.DirectorySecurity
    $protected.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)')
    [IO.Directory]::CreateDirectory($badBase, $protected) | Out-Null
    Assert-Protected $badBase
    $refused = $false
    try { Use-ProtectedFolder (Join-Path $badBase 'Latch\Setup') $badBase } catch { $refused = $true }
    if (!$refused -or (Test-Path -LiteralPath (Join-Path $badBase 'Latch'))) { throw 'Accepted a protected base below a writable ancestor' }
    # The handles survive the helper and prevent renaming a checked parent.
    $pins = @{}
    $pinned = Join-Path $root 'pinned'
    $renamed = Join-Path $root 'renamed'
    try {
        Use-ProtectedFolder (Join-Path $pinned 'Setup') $root $pins
        $refused = $false
        try { [IO.Directory]::Move($pinned, $renamed) } catch { $refused = $true }
        if (!$refused) { throw 'Renamed a pinned directory' }
    } finally {
        foreach ($handle in $pins.Values) { $handle.Dispose() }
    }
    [IO.Directory]::Move($pinned, $renamed)
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

    # A writable ancestor is moved as a unit, and the new tree cannot
    # inherit any files or permissions from it.
    $repaired = Join-Path $root 'Latch'
    New-Item -ItemType Directory -Path $repaired | Out-Null
    $acl = Get-Acl -LiteralPath $repaired
    $acl.SetOwner($me)
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($users, 'Modify', 'ContainerInherit, ObjectInherit', 'None', 'Allow')))
    Set-Acl -LiteralPath $repaired -AclObject $acl
    Set-Content -LiteralPath (Join-Path $repaired 'keep-old.txt') -Value 'old tree'
    Must-Reject $repaired
    Use-ProtectedFolder (Join-Path $repaired 'Input') $root
    $quarantine = @(Get-ChildItem -LiteralPath $root -Filter 'Latch.untrusted-*')
    if ($quarantine.Count -ne 1 -or !(Test-Path -LiteralPath (Join-Path $quarantine[0].FullName 'keep-old.txt'))) { throw 'Did not quarantine the ancestor as a unit' }
    if (Test-Path -LiteralPath (Join-Path $repaired 'keep-old.txt')) { throw 'Reused the old ancestor' }
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
    # A junction inside the old tree is quarantined without following it.
    New-Item -ItemType Junction -Path (Join-Path $inputDir 'out') -Target $unsafe | Out-Null
    Set-Content -LiteralPath (Join-Path $unsafe 'keep.txt') -Value 'kept'
    Reset-ProtectedFolder $inputDir $root
    if (Test-Path -LiteralPath $planted) { throw 'Kept a planted file' }
    if (Get-ChildItem -LiteralPath $inputDir -Force) { throw 'The folder was not made again' }
    if ((Get-Content -LiteralPath (Join-Path $unsafe 'keep.txt')) -ne 'kept') { throw 'Followed a junction' }
    if (-not (Test-ProtectedContents $inputDir)) { throw 'The new folder is not protected' }
    $quarantine = @(Get-ChildItem -LiteralPath $repaired -Filter 'Input.untrusted-*')
    if ($quarantine.Count -ne 1) { throw 'Did not quarantine the unsafe contents' }
    [IO.Directory]::Delete((Join-Path $quarantine[0].FullName 'out'))
    # A junction at the original root can be removed without recursion.
    New-Item -ItemType Junction -Path $junction -Target $unsafe | Out-Null
    Use-ProtectedFolder $junction $root
    Assert-Protected $junction
    if (Get-ChildItem -LiteralPath $root -Filter 'junction.untrusted-*') { throw 'Did not remove the quarantined junction entry' }
    if ((Get-Content -LiteralPath (Join-Path $unsafe 'keep.txt')) -ne 'kept') { throw 'Removed the junction target' }
    # If Windows refuses the rename, leave the old entry and fail closed.
    $blocked = Join-Path $root 'blocked'
    $held = [IO.File]::Open($blocked, 'CreateNew', 'ReadWrite', 'Read')
    try {
        $refused = $false
        try { Use-ProtectedFolder (Join-Path $blocked 'Child') $root } catch { $refused = $true }
        if (!$refused -or !(Test-Path -LiteralPath $blocked -PathType Leaf)) { throw 'Continued after a blocked rename' }
    } finally { $held.Dispose() }
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
