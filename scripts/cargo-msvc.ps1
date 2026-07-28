# Repository-local Cargo launcher for the statically linked MSVC target.
#
# This deliberately constructs the small environment surface rustc/link.exe
# need instead of importing a user's interactive cmd.exe AutoRun hooks through
# VsDevCmd.bat. It discovers installed versions, so servicing updates do not
# require editing this file.
$ErrorActionPreference = 'Stop'
$CargoArguments = $args

$vsRoot = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\2022\BuildTools'
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
