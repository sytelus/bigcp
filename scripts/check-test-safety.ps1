# Mechanical backstop for prohibited storage operations (VISION.md test guidance).
# This script is a *backstop*: the primary enforcement is structural (testkit
# sandbox guard, generator entry/byte caps). It scans every Rust source, build
# script, scenario file, and repo script except itself for operation names that
# have no legitimate use anywhere in this repository.
$ErrorActionPreference = 'Stop'
$Self = (Resolve-Path $MyInvocation.MyCommand.Path).Path
$ScanFiles = @(
    Get-ChildItem -LiteralPath 'crates' -Recurse -File -Include '*.rs'
    Get-ChildItem -LiteralPath 'testkit' -Recurse -File -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath 'scripts' -Recurse -File -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath '.github' -Recurse -File -ErrorAction SilentlyContinue
    Get-ChildItem -Filter 'build.rs' -Recurse -File -ErrorAction SilentlyContinue
) | Where-Object {
    # target\ holds third-party build scripts this repository does not control;
    # only sources the repo owns are subject to the backstop.
    (Resolve-Path $_.FullName).Path -ne $Self -and
    $_.FullName -notmatch '\\target\\'
}

# Plain substrings (no regex fragility): none of these has a legitimate use in
# this repo in any spelling, escaped or raw.
$ForbiddenSubstrings = @(
    'PhysicalDrive'          # raw disk access, any string spelling
    'diskpart'
    'fsutil'                 # volume/filesystem mutation utility
    'Format-Volume'
    'Clear-Disk'
    'Initialize-Disk'
    'New-Partition'
    'New-Volume'
    'Set-Partition'
    'Remove-Partition'
    'Mount-VHD'
    'Dismount-VHD'
    'New-VHD'
    'mountvol /d'            # volume unmount
    'FSCTL_DISMOUNT_VOLUME'
    'shutdown /'             # machine-stability operations
    'Restart-Computer'
    'Stop-Computer'
)

foreach ($file in $ScanFiles) {
    $text = Get-Content -Raw -LiteralPath $file.FullName
    foreach ($needle in $ForbiddenSubstrings) {
        if ($text.IndexOf($needle, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "Prohibited test-storage operation '$needle' in $($file.FullName)"
        }
    }
}

# Anchored structural assertions (non-comment code lines, not mere substrings).
function Assert-CodeLine {
    param([string]$Path, [string]$Pattern, [string]$Message)
    $hit = Get-Content -LiteralPath $Path | Where-Object {
        $_ -match $Pattern -and $_.TrimStart() -notmatch '^(//|#)'
    }
    if (-not $hit) { throw $Message }
}

Assert-CodeLine 'crates/testkit/src/sandbox.rs' `
    'fn allowed_test_drives' `
    'Sandbox guard no longer declares the system+code drive whitelist.'
# The cap assertions pin the literal values, not just the identifiers:
# silently raising a limit is the same regression as deleting it.
Assert-CodeLine 'crates/testkit/src/generator.rs' `
    'HARD_ENTRY_LIMIT: usize = 10_000;' `
    'Generator hard entry cap is missing or no longer 10,000 (VISION scale prohibition).'
Assert-CodeLine 'crates/testkit/src/generator.rs' `
    'HARD_WRITE_LIMIT: u64 = 1024 \* 1024 \* 1024;' `
    'Generator hard write cap is missing or no longer 1 GiB.'
Assert-CodeLine 'crates/testkit/src/generator.rs' `
    'ROUTINE_ENTRY_LIMIT: usize = 1_000;' `
    'Generator routine entry cap is missing, no longer 1,000, or no longer separates routine caps from opt-in heavy scenarios.'
Assert-CodeLine 'crates/testkit/src/generator.rs' `
    'ROUTINE_WRITE_LIMIT: u64 = 64 \* 1024 \* 1024;' `
    'Generator routine write cap is missing or no longer 64 MiB.'
Assert-CodeLine 'crates/testkit/src/generator.rs' `
    'fn planned_entry_counts' `
    'Generator no longer counts implicit parent directories before enforcing entry caps.'

# The heavy-test opt-in must come from the operator's environment, never from
# code. No Rust source in this repository may set environment variables.
$RustFiles = $ScanFiles | Where-Object { $_.Extension -eq '.rs' }
foreach ($file in $RustFiles) {
    $text = Get-Content -Raw -LiteralPath $file.FullName
    if ($text.Contains('set_var')) {
        throw "Environment mutation 'set_var' in $($file.FullName): the heavy-test opt-in and other env vars must be set by the operator, not by code"
    }
}
Assert-CodeLine 'crates/core/tests/end_to_end.rs' `
    'validated_system_temp' `
    'End-to-end tests no longer validate the system temporary root before writes.'
Assert-CodeLine 'crates/core/tests/end_to_end.rs' `
    'state_dir' `
    'End-to-end tests no longer confine run state to the sandbox.'

Write-Output 'Automated test storage-safety checks passed.'
