<#
.SYNOPSIS
  Build, sign and package EffectCraft for Windows.

.DESCRIPTION
  Produces, in $env:DIST (default: dist/release):
    effectcraft-<version>-windows-<arch>.msi            per-machine installer (WiX v5)
    effectcraft-<version>-windows-<arch>-portable.zip   effectcraft.exe + effectcraft-cli.exe + portable.txt

  The binaries link the C runtime statically (+crt-static), so neither the MSI nor the portable
  zip needs the Visual C++ redistributable. Signing is delegated to sign.ps1 (skipped with a
  warning when no signing secrets are set).

  Needs: Rust (MSVC toolchain + the target), the Windows SDK (rc.exe, signtool.exe),
  and WiX v5: dotnet tool install --global wix --version 5.0.2

.EXAMPLE
  pwsh packaging/windows/package.ps1 -Arch x64
  pwsh packaging/windows/package.ps1 -Arch x86 -SkipBuild
  pwsh packaging/windows/package.ps1 -Arch arm64     # cross-compiled; needs the MSVC ARM64 build tools
#>
param(
  [ValidateSet('x64', 'x86', 'arm64')] [string] $Arch = 'x64',
  [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Invoke-Native([string] $What, [scriptblock] $Block) {
  Write-Output "==> $What"
  & $Block
  if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

# The version lives in one place: [workspace.package] version in the root Cargo.toml.
$Version = $env:EFFECTCRAFT_VERSION
if (-not $Version) {
  $inPkg = $false
  foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
    if ($line -match '^\s*\[') { $inPkg = ($line.Trim() -eq '[workspace.package]'); continue }
    if ($inPkg -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw 'could not read [workspace.package] version from Cargo.toml' }
# MSI ProductVersion is numeric (major.minor.build); pre-release tags are dropped there.
$MsiVersion = ($Version -split '-')[0]

$Target = switch ($Arch) { 'x64' { 'x86_64-pc-windows-msvc' } 'x86' { 'i686-pc-windows-msvc' } 'arm64' { 'aarch64-pc-windows-msvc' } }
$Dist = if ($env:DIST) { $env:DIST } else { Join-Path $Root 'dist\release' }
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

if (-not $env:EFFECTCRAFT_BUILD_SHA) { $env:EFFECTCRAFT_BUILD_SHA = (git -C $Root rev-parse HEAD 2>$null) }
if (-not $env:EFFECTCRAFT_BUILD_DATE) { $env:EFFECTCRAFT_BUILD_DATE = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd') }

Write-Output "EffectCraft $Version for Windows $Arch ($Target)"

if (-not $SkipBuild) {
  # Static CRT: no VC++ redistributable needed. Scoped to the target so host build scripts and
  # proc-macros are unaffected.
  $flagVar = 'CARGO_TARGET_' + ($Target.ToUpper() -replace '-', '_') + '_RUSTFLAGS'
  [Environment]::SetEnvironmentVariable($flagVar, '-C target-feature=+crt-static')
  # Fail the build (rather than warn) if the icon/VERSIONINFO can't be embedded.
  $env:EFFECTCRAFT_REQUIRE_WINRES = '1'
  Invoke-Native "cargo build ($Target)" { cargo build --release --locked -p effectcraft -p effectcraft-cli --target $Target }
}

$Bin = Join-Path $TargetDir "$Target\release"

# Check both binaries' PE headers before packaging. Machine (COFF header) must match -Arch, so an
# x64 build can never ship labelled arm64. Subsystem (optional header): 2 = Windows GUI, 3 = console.
# The app must be GUI (no console window opens with it); the CLI must stay console so its output
# reaches the terminal.
function Get-PeHeader([string] $Path) {
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $pe = [BitConverter]::ToInt32($bytes, 0x3C)
  return @{ Machine = [BitConverter]::ToUInt16($bytes, $pe + 4); Subsystem = [BitConverter]::ToUInt16($bytes, $pe + 0x5C) }
}
$Machine = switch ($Arch) { 'x64' { 0x8664 } 'x86' { 0x14C } 'arm64' { 0xAA64 } }
foreach ($check in @(@('effectcraft.exe', 2), @('effectcraft-cli.exe', 3))) {
  $h = Get-PeHeader (Join-Path $Bin $check[0])
  if ($h.Machine -ne $Machine) { throw "$($check[0]) is for machine 0x$('{0:X}' -f $h.Machine), expected 0x$('{0:X}' -f $Machine) ($Arch)" }
  if ($h.Subsystem -ne $check[1]) { throw "$($check[0]) has PE subsystem $($h.Subsystem), expected $($check[1])" }
  Write-Output "ok $($check[0]): $Arch, PE subsystem $($h.Subsystem)"
}
$Stage = Join-Path $TargetDir "windows-package\$Arch"
Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item (Join-Path $Bin 'effectcraft.exe'), (Join-Path $Bin 'effectcraft-cli.exe') $Stage

# DXC is the maintained DX12 shader compiler. Pin Microsoft's official release and verify it
# before extraction so packaging never silently ships a changed or partial download.
# The official runtime is distributed for x64 and ARM64. x86 keeps wgpu's automatic compiler path.
$IncludeDxc = $false
if ($Arch -ne 'x86') {
$DxcVersion = '1.9.2609'
$DxcArchiveName = 'dxc_2026_09_29.zip'
$DxcUri = "https://github.com/microsoft/DirectXShaderCompiler/releases/download/v$DxcVersion/$DxcArchiveName"
$DxcSha256 = 'ad31b1fc8443175d204f77a611fdb3ef2ec42759bdc2f1167368de24a4a7e7f1'
New-Item -ItemType Directory -Force -Path $TargetDir | Out-Null
$DxcArchive = Join-Path $TargetDir $DxcArchiveName
if (Test-Path -LiteralPath $DxcArchive) {
  $existingHash = (Get-FileHash -Path $DxcArchive -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($existingHash -ne $DxcSha256) {
    Remove-Item -Force $DxcArchive
  }
}
if (-not (Test-Path -LiteralPath $DxcArchive)) {
  Write-Output "Downloading Microsoft's DXC $DxcVersion runtime"
  Invoke-WebRequest -Uri $DxcUri -OutFile $DxcArchive
}
$actualHash = (Get-FileHash -Path $DxcArchive -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $DxcSha256) {
  Remove-Item -Force $DxcArchive -ErrorAction SilentlyContinue
  throw "DXC archive SHA-256 mismatch: expected $DxcSha256, received $actualHash"
}

$DxcExtract = Join-Path $TargetDir "dxc-$DxcVersion"
Remove-Item -Recurse -Force $DxcExtract -ErrorAction SilentlyContinue
Expand-Archive -Path $DxcArchive -DestinationPath $DxcExtract -Force
# Don't assume Microsoft's ZIP uses a particular directory naming scheme; select the native
# binaries by their PE machine type so renamed/reshuffled folders cannot select the wrong DLL.
$DxcCompiler = Get-ChildItem -Path $DxcExtract -Recurse -File -Filter 'dxcompiler.dll' |
  Where-Object { (Get-PeHeader $_.FullName).Machine -eq $Machine } | Select-Object -First 1
if (-not $DxcCompiler) { throw "Official DXC $DxcVersion has no dxcompiler.dll for Windows $Arch (PE machine 0x$('{0:X}' -f $Machine))" }
$DxcValidator = Get-ChildItem -Path $DxcExtract -Recurse -File -Filter 'dxil.dll' |
  Where-Object { (Get-PeHeader $_.FullName).Machine -eq $Machine } | Select-Object -First 1
if (-not $DxcValidator) { throw "Official DXC $DxcVersion has no dxil.dll for Windows $Arch (PE machine 0x$('{0:X}' -f $Machine))" }
Copy-Item $DxcCompiler.FullName (Join-Path $Stage 'dxcompiler.dll')
Copy-Item $DxcValidator.FullName (Join-Path $Stage 'dxil.dll')

# Ship the DXC license and third-party notices in both the MSI and portable distribution.
$DxcLicenseFiles = @(
  @{
    SourceName = 'LICENSE.TXT'
    OutputName = 'DirectXShaderCompiler-LICENSE.txt'
    Url = "https://raw.githubusercontent.com/microsoft/DirectXShaderCompiler/v$DxcVersion/LICENSE.TXT"
  },
  @{
    SourceName = 'ThirdPartyNotices.txt'
    OutputName = 'DirectXShaderCompiler-ThirdPartyNotices.txt'
    Url = "https://raw.githubusercontent.com/microsoft/DirectXShaderCompiler/v$DxcVersion/ThirdPartyNotices.txt"
  }
)
foreach ($license in $DxcLicenseFiles) {
  $source = Get-ChildItem -Path $DxcExtract -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ieq $license.SourceName } | Select-Object -First 1
  $destination = Join-Path $Stage $license.OutputName
  if ($source) {
    Copy-Item $source.FullName $destination
  } else {
    Invoke-WebRequest -Uri $license.Url -OutFile $destination
  }
}
$IncludeDxc = $true

} else {
  Write-Warning 'No bundled DXC runtime is provided for x86; wgpu automatic compiler selection remains enabled.'
}

& (Join-Path $PSScriptRoot 'sign.ps1') (Join-Path $Stage 'effectcraft.exe') (Join-Path $Stage 'effectcraft-cli.exe')

# Required embedded-font notices ship independently of optional font inputs.
Copy-Item -LiteralPath (Join-Path $Root 'NOTICE') -Destination $Stage

# ---- MSI ---------------------------------------------------------------------------------------
$Msi = Join-Path $Dist "effectcraft-$Version-windows-$Arch.msi"
Invoke-Native 'wix build' {
  wix build (Join-Path $PSScriptRoot 'effectcraft.wxs') (Join-Path $PSScriptRoot 'installer-ui.wxs') -arch $Arch `
    -d "Version=$MsiVersion" -d "BinDir=$Stage" -d "IncludeDxc=$([int]$IncludeDxc)" -d "IconPath=$(Join-Path $Root 'assets\app-icon\effectcraft.ico')" `
    -o $Msi
}
# Check the compiled scope, path and registration contract before signing.
& (Join-Path $PSScriptRoot 'check-install-policy.ps1') -MsiPath $Msi
# wix writes its debug symbols (.wixpdb) next to the MSI; keep them out of the release assets.
Remove-Item -Force -ErrorAction SilentlyContinue ([IO.Path]::ChangeExtension($Msi, '.wixpdb'))
& (Join-Path $PSScriptRoot 'sign.ps1') $Msi

# ---- portable zip ------------------------------------------------------------------------------
$Portable = Join-Path $TargetDir "windows-package\effectcraft-$Version-windows-$Arch-portable"
Remove-Item -Recurse -Force $Portable -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Portable | Out-Null
Copy-Item (Join-Path $Stage '*.exe') $Portable
if ($IncludeDxc) {
  Copy-Item (Join-Path $Stage 'dxcompiler.dll'), (Join-Path $Stage 'dxil.dll'), (Join-Path $Stage 'DirectXShaderCompiler-LICENSE.txt'), (Join-Path $Stage 'DirectXShaderCompiler-ThirdPartyNotices.txt') $Portable
}
foreach ($f in 'README.md', 'LICENSE', 'LICENSE-MIT', 'LICENSE-APACHE', 'NOTICE') {
  $p = Join-Path $Root $f
  if (Test-Path $p) { Copy-Item $p $Portable }
}
# portable.txt beside effectcraft.exe switches on portable mode: settings, shortcuts and recovery
# files go to EffectCraftData\ next to the exe instead of %APPDATA% (#993; see app_dirs.rs).
Copy-Item (Join-Path $PSScriptRoot 'portable.txt') $Portable
$Zip = Join-Path $Dist "effectcraft-$Version-windows-$Arch-portable.zip"
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
Compress-Archive -Path $Portable -DestinationPath $Zip

# Smoke-test the CLI when this machine can run it. An ARM64 build made on an x64 runner can't run
# here; .github/workflows/windows-arm64.yml installs and runs it on ARM64 instead.
$HostArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($Arch -ne 'arm64' -or $HostArch -eq 'arm64') {
  Invoke-Native 'effectcraft-cli --version' { & (Join-Path $Stage 'effectcraft-cli.exe') --version }
} else {
  Write-Output "skipping effectcraft-cli --version: an $Arch build doesn't run on this $HostArch machine"
}
Get-Item $Msi, $Zip | Format-Table Name, Length
