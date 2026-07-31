# Repository-local Cargo launcher for the statically linked MSVC target.
#
# This deliberately constructs the small environment surface rustc/link.exe
# need instead of importing a user's interactive cmd.exe AutoRun hooks through
# VsDevCmd.bat. It discovers installed versions, so servicing updates do not
# require editing this file.
$ErrorActionPreference = 'Stop'
$CargoArguments = $args

# Discover the Visual Studio instance instead of hard-coding one SKU path:
# local machines typically carry Build Tools while CI images carry Enterprise.
# vswhere.exe is installed at this fixed path by every Visual Studio SKU; the
# literal 2022 paths remain as a fallback for images without it.
$vsRoot = $null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path -LiteralPath $vswhere) {
    $vsRoot = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath | Select-Object -First 1
}
if ([string]::IsNullOrWhiteSpace($vsRoot)) {
    $vsRoot = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\2022\BuildTools'),
        (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\2022\Enterprise'),
        (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\2022\Professional'),
        (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\2022\Community')
    ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
}
if ([string]::IsNullOrWhiteSpace($vsRoot)) {
    throw 'No Visual Studio instance with the x64 VC tools was found (checked vswhere and the known 2022 install paths)'
}
$msvcRoot = Join-Path $vsRoot 'VC\Tools\MSVC'
$msvc = Get-ChildItem -LiteralPath $msvcRoot -Directory |
    Sort-Object -Property Name -Descending |
    Select-Object -First 1
if ($null -eq $msvc) {
    throw "No Visual Studio 2022 MSVC toolset was found under $msvcRoot"
}

$kitsRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$sdk = Get-ChildItem -LiteralPath (Join-Path $kitsRoot 'Lib') -Directory |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'um\x64\kernel32.lib') } |
    Sort-Object -Property Name -Descending |
    Select-Object -First 1
if ($null -eq $sdk) {
    throw "No x64 Windows SDK was found under $kitsRoot"
}

$sdkVersion = $sdk.Name
$msvcPath = $msvc.FullName
$sdkInclude = Join-Path $kitsRoot "Include\$sdkVersion"
$sdkLib = Join-Path $kitsRoot "Lib\$sdkVersion"
$toolPaths = @(
    (Join-Path $msvcPath 'bin\Hostx64\x64'),
    (Join-Path $kitsRoot "bin\$sdkVersion\x64")
)
$env:Path = ($toolPaths + $env:Path) -join [IO.Path]::PathSeparator
$env:INCLUDE = @(
    (Join-Path $msvcPath 'include'),
    (Join-Path $sdkInclude 'ucrt'),
    (Join-Path $sdkInclude 'shared'),
    (Join-Path $sdkInclude 'um'),
    (Join-Path $sdkInclude 'winrt'),
    (Join-Path $sdkInclude 'cppwinrt')
) -join [IO.Path]::PathSeparator
$env:LIB = @(
    (Join-Path $msvcPath 'lib\x64'),
    (Join-Path $sdkLib 'ucrt\x64'),
    (Join-Path $sdkLib 'um\x64')
) -join [IO.Path]::PathSeparator
$env:LIBPATH = @(
    (Join-Path $msvcPath 'lib\x64'),
    (Join-Path $sdkLib 'ucrt\x64'),
    (Join-Path $sdkLib 'um\x64')
) -join [IO.Path]::PathSeparator
$env:VCToolsInstallDir = "$msvcPath\"
$env:WindowsSdkDir = "$kitsRoot\"
$env:WindowsSDKVersion = "$sdkVersion\"

$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
& $cargo @CargoArguments
exit $LASTEXITCODE
