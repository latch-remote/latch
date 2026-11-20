$env:PATH = [Environment]::SystemDirectory
$env:PSModulePath = [IO.Path]::Combine([Environment]::SystemDirectory, 'WindowsPowerShell\v1.0\Modules')
Set-Location -LiteralPath ([Environment]::SystemDirectory)
# Run only elevated. Refuse pre-existing paths a non-administrator can
# redirect or change; changing their ACL would leave existing handles open.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Setup requires an administrator'
}
# What setup writes runs elevated or as SYSTEM, so only these may own it or
# change it. Not the account running setup: an administrator's own SID is
# also the SID of their unelevated processes.
$trusted = @('S-1-5-18', 'S-1-5-32-544',
    'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464') # TrustedInstaller
# Rights that change a file or folder or what it holds: write, append (add
# a subfolder), extended attributes, attributes, delete child, delete,
# change permissions, take ownership, and the generic all and write rights
# an ACE may carry instead.
$writeRights = 0x2 -bor 0x4 -bor 0x10 -bor 0x100 -bor 0x40 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x10000000 -bor 0x40000000
# Why $path is not protected, or nothing when it is.
function Test-Protected($path, [switch]$Ancestor) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { return "Reparse point: $path" }
    $acl = Get-Acl -LiteralPath $path -ErrorAction Stop
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $trusted) { return "Untrusted owner: $path" }
    $rights = $writeRights
    # An NTFS volume root can let users create their own top-level entries.
    # Those rights cannot modify an existing entry. DELETE_CHILD still fails.
    if ($Ancestor -and $path -eq [IO.Path]::GetPathRoot($path)) { $rights = $rights -band (-bnot 0x6) }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        # CREATOR OWNER is replaced by the owner of the child. A fixed
        # untrusted SID can still make a new child writable via inheritance.
        if (($Ancestor -or $rule.IdentityReference.Value -eq 'S-1-3-0') -and
            ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly)) { continue }
        if ($rule.AccessControlType -eq 'Allow' -and (([int64]$rule.FileSystemRights -band 0xFFFFFFFFL) -band $rights) -and $rule.IdentityReference.Value -notin $trusted) {
            return "Untrusted write access: $path"
        }
    }
}
function Assert-Protected($path, [switch]$Ancestor) {
    $why = Test-Protected $path -Ancestor:$Ancestor
    if ($why) { throw $why }
}
function Assert-ProtectedTree($path) {
    $path = [IO.Path]::GetFullPath($path)
    if ($path.StartsWith('\\')) { throw 'Setup requires a local path' }
    $root = [IO.Path]::GetPathRoot($path)
    $walk = $root
    Assert-Protected $root -Ancestor
    foreach ($part in $path.Substring($root.Length).Split('\')) {
        if (-not $part) { continue }
        $walk = Join-Path $walk $part
        Assert-Protected $walk
    }
}
# Gives a folder or file setup just created back to administrators:
# Administrators own it, and it has only what it inherits from its parent,
# which the caller checked first. Never a reparse point. Then checks again.
function Repair-Protected($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point: $path" }
    $system = [Environment]::SystemDirectory
    & (Join-Path $system 'takeown.exe') /A /F $path | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not make Administrators the owner of $path" }
    & (Join-Path $system 'icacls.exe') $path /reset /C /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not reset the permissions of $path" }
    Assert-Protected $path
}
# The parent is protected. Move this entry as a unit without opening any
# descendants, and fail if Windows cannot rename it (for example, an open
# handle denies delete sharing).
function Move-UntrustedTree($item) {
    $path = $item.FullName
    $quarantine = $path + '.untrusted-' + [guid]::NewGuid().ToString('N')
    if ($item.PSIsContainer) {
        [IO.Directory]::Move($path, $quarantine)
    } else {
        [IO.File]::Move($path, $quarantine)
    }
    # A nonrecursive delete removes a junction or symlink itself. Leave
    # nonempty directories quarantined: old handles may still modify
    # their contents, including changing a directory into a junction.
    try {
        if ($item.PSIsContainer) { [IO.Directory]::Delete($quarantine) }
        else { [IO.File]::Delete($quarantine) }
    } catch {}
}
# Emit the P/Invoke in memory. Add-Type's compiler uses a writable temp
# directory, which is unsuitable before we have pinned a protected path.
function Open-ProtectedDirectory($path, $pins, [switch]$Ancestor) {
    if ($pins.ContainsKey($path)) { return }
    if (!$script:directoryPinType) {
        $name = New-Object Reflection.AssemblyName 'LatchDirectoryPin'
        $assembly = [AppDomain]::CurrentDomain.DefineDynamicAssembly($name, 'Run')
        $type = $assembly.DefineDynamicModule('Native').DefineType('LatchDirectoryPin', 'Public')
        $method = $type.DefinePInvokeMethod('CreateFileW', 'kernel32.dll', 'Public,Static,PinvokeImpl',
            'Standard', [Microsoft.Win32.SafeHandles.SafeFileHandle],
            [Type[]]@([string], [uint32], [uint32], [IntPtr], [uint32], [uint32], [IntPtr]),
            'Winapi', 'Unicode')
        $method.SetImplementationFlags($method.GetMethodImplementationFlags() -bor [Reflection.MethodImplAttributes]::PreserveSig)
        $script:directoryPinType = $type.CreateType()
    }
    # LIST_DIRECTORY | READ_ATTRIBUTES, share READ and WRITE, OPEN_EXISTING,
    # BACKUP_SEMANTICS
    # and OPEN_REPARSE_POINT. Open the entry itself and deny its rename
    # or deletion while we validate and use it.
    $handle = $script:directoryPinType::CreateFileW($path, 0x81, 3, [IntPtr]::Zero, 3, 0x02200000, [IntPtr]::Zero)
    try {
        if ($handle.IsInvalid) { throw "Could not pin directory: $path" }
        if (!(Get-Item -LiteralPath $path -Force -ErrorAction Stop).PSIsContainer) { throw "Not a directory: $path" }
        Assert-Protected $path -Ancestor:$Ancestor
        $pins[$path] = $handle
    } finally {
        if (!$pins.ContainsKey($path)) { $handle.Dispose() }
    }
}
# Walk the base from the drive root. Repair entries only below that base,
# under a pinned, protected parent.
function Use-ProtectedFolder($path, $base = [Environment]::GetFolderPath('ProgramFiles'), $pins) {
    $base = [IO.Path]::GetFullPath($base).TrimEnd('\')
    $path = [IO.Path]::GetFullPath($path).TrimEnd('\')
    if ($base.StartsWith('\\')) { throw 'Setup requires a local path' }
    if ($path -ine $base -and -not $path.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "Not under ${base}: $path" }
    $ownPins = $null -eq $pins
    if ($ownPins) { $pins = @{} }
    try {
        $root = [IO.Path]::GetPathRoot($base + '\')
        $walk = $root
        Open-ProtectedDirectory $walk $pins -Ancestor
        foreach ($part in ($base + '\').Substring($root.Length).Split('\')) {
            if (!$part) { continue }
            $walk = Join-Path $walk $part
            Open-ProtectedDirectory $walk $pins -Ancestor
        }
        foreach ($part in $path.Substring($base.Length).Split('\')) {
            if (!$part) { continue }
            $walk = Join-Path $walk $part
            $item = try { Get-Item -LiteralPath $walk -Force -ErrorAction Stop } catch [System.Management.Automation.ItemNotFoundException] {}
            if (!$item -or !$item.PSIsContainer -or (Test-Protected $walk)) {
                if ($item) { Move-UntrustedTree $item }
                $acl = New-Object Security.AccessControl.DirectorySecurity
                $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)')
                [IO.Directory]::CreateDirectory($walk, $acl) | Out-Null
            }
            Open-ProtectedDirectory $walk $pins
        }
    } finally {
        if ($ownPins) { foreach ($handle in $pins.Values) { $handle.Dispose() } }
    }
}
# Whether $path and everything in it is protected.
function Test-ProtectedContents($path) {
    if (Test-Protected $path) { return $false }
    foreach ($child in Get-ChildItem -LiteralPath $path -Force -ErrorAction Stop) {
        if ($child.PSIsContainer) {
            if (-not (Test-ProtectedContents $child.FullName)) { return $false }
        } elseif (Test-Protected $child.FullName) { return $false }
    }
    return $true
}
# Called under a protected parent. Recurse only through a protected tree.
function Remove-Tree($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if (!$item.PSIsContainer -or !(Test-ProtectedContents $path)) {
        Move-UntrustedTree $item
        return
    }
    Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop
}
# A folder setup fills completely, such as the SYSTEM helper's: anything in
# it that is not protected may have been planted, and a folder that was not
# may still be open to whoever could change it. So it is not repaired but
# made again, empty and protected.
function Reset-ProtectedFolder($path, $base = [Environment]::GetFolderPath('ProgramFiles'), $pins) {
    $ownPins = $null -eq $pins
    if ($ownPins) { $pins = @{} }
    try {
        Use-ProtectedFolder (Split-Path -Parent $path) $base $pins
        $item = try { Get-Item -LiteralPath $path -Force -ErrorAction Stop } catch [System.Management.Automation.ItemNotFoundException] {}
        if ($item -and (!$item.PSIsContainer -or !(Test-ProtectedContents $path))) { Move-UntrustedTree $item }
        Use-ProtectedFolder $path $base $pins
    } finally {
        if ($ownPins) { foreach ($handle in $pins.Values) { $handle.Dispose() } }
    }
}
