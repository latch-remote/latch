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
function Test-Protected($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { return "Reparse point: $path" }
    $acl = Get-Acl -LiteralPath $path -ErrorAction Stop
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $trusted) { return "Untrusted owner: $path" }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) { continue }
        if ($rule.AccessControlType -eq 'Allow' -and (([int64]$rule.FileSystemRights -band 0xFFFFFFFFL) -band $writeRights) -and $rule.IdentityReference.Value -notin $trusted) {
            return "Untrusted write access: $path"
        }
    }
}
function Assert-Protected($path) {
    $why = Test-Protected $path
    if ($why) { throw $why }
}
function Assert-ProtectedTree($path) {
    $path = [IO.Path]::GetFullPath($path)
    if ($path.StartsWith('\\')) { throw 'Setup requires a local path' }
    $root = [IO.Path]::GetPathRoot($path)
    $walk = $root
    foreach ($part in $path.Substring($root.Length).Split('\')) {
        if (-not $part) { continue }
        $walk = Join-Path $walk $part
        Assert-Protected $walk
    }
}
# Gives a folder or file setup made back to administrators, in place:
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
# Makes sure every folder below $base (which must already be protected,
# and is never repaired) down to $path exists and is protected, creating or
# repairing each in turn. $base is %ProgramFiles% unless a test names
# another.
function Use-ProtectedFolder($path, $base = [Environment]::GetFolderPath('ProgramFiles')) {
    $base = [IO.Path]::GetFullPath($base).TrimEnd('\')
    $path = [IO.Path]::GetFullPath($path).TrimEnd('\')
    if ($base.StartsWith('\\')) { throw 'Setup requires a local path' }
    if (-not $path.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "Not under ${base}: $path" }
    Assert-Protected $base
    $walk = $base
    foreach ($part in $path.Substring($base.Length).Split('\')) {
        if (-not $part) { continue }
        $walk = Join-Path $walk $part
        if (-not (Test-Path -LiteralPath $walk)) { New-Item -ItemType Directory -Path $walk -ErrorAction Stop | Out-Null }
        if (Test-Protected $walk) { Repair-Protected $walk }
    }
}
# Whether $path and everything in it is protected.
function Test-ProtectedContents($path) {
    if (Test-Protected $path) { return $false }
    foreach ($child in Get-ChildItem -LiteralPath $path -Recurse -Force -ErrorAction Stop) {
        if (Test-Protected $child.FullName) { return $false }
    }
    return $true
}
# Deletes $path and what it holds. A reparse point (a junction, a link) is
# removed itself, never followed.
function Remove-Tree($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        if ($item.PSIsContainer) { [IO.Directory]::Delete($path) } else { [IO.File]::Delete($path) }
        return
    }
    if ($item.PSIsContainer) {
        foreach ($child in Get-ChildItem -LiteralPath $path -Force -ErrorAction Stop) { Remove-Tree $child.FullName }
        [IO.Directory]::Delete($path)
    } else {
        $item.Attributes = 'Normal'
        [IO.File]::Delete($path)
    }
}
# A folder setup fills completely, such as the SYSTEM helper's: anything in
# it that is not protected may have been planted, and a folder that was not
# may still be open to whoever could change it. So it is not repaired but
# made again, empty and protected.
function Reset-ProtectedFolder($path, $base = [Environment]::GetFolderPath('ProgramFiles')) {
    Use-ProtectedFolder (Split-Path -Parent $path) $base
    if ((Test-Path -LiteralPath $path) -and -not (Test-ProtectedContents $path)) { Remove-Tree $path }
    Use-ProtectedFolder $path $base
}
