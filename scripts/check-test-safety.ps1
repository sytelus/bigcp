# Mechanical backstop for prohibited storage operations in automated tests.
$ErrorActionPreference = 'Stop'
$TestFiles = @(
    Get-ChildItem -LiteralPath 'crates' -Recurse -File -Include '*.rs'
    Get-ChildItem -LiteralPath '.github' -Recurse -File -ErrorAction SilentlyContinue
)
$Forbidden = @(
    '\\\\.\\PhysicalDrive'
    '\bdiskpart\b'
    '\bFormat-Volume\b'
    '\bClear-Disk\b'
    '\bInitialize-Disk\b'
    '\bDismount-VHD\b'
)

foreach ($file in $TestFiles) {
    $text = Get-Content -Raw -LiteralPath $file.FullName
    foreach ($pattern in $Forbidden) {
        if ($text -match $pattern) {
            throw "Prohibited test-storage pattern '$pattern' in $($file.FullName)"
        }
    }
}

$sandbox = Get-Content -Raw -LiteralPath 'crates/testkit/src/sandbox.rs'
foreach ($drive in @("'F'", "'G'", "'H'")) {
    if (-not $sandbox.Contains($drive)) {
        throw "Sandbox guard no longer rejects drive $drive"
    }
}

$integration = Get-Content -Raw -LiteralPath 'crates/core/tests/end_to_end.rs'
if (-not $integration.Contains('validated_system_temp')) {
    throw 'End-to-end tests do not validate the system temporary root before writes.'
}

Write-Output 'Automated test storage-safety checks passed.'
