# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-30
# after the owner-authorized same-spindle transport and policy change updated
# VISION.md, PLAN.md, and LIMITATIONS.md together with ADR 0036.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'DFE35C96636EBBE3E9F3A361BF169EFE567C9D64B35020CECF8A44EFF1B42BD2'
    'VISION.md' = '739678D0602FEF55D19A988D9FD60B17E21EA4A4EC59D9094F9DDD90A38CB678'
    'LIMITATIONS.md' = '8FA86A1283BF8B303BBC9713D1A9B61213E23333B9320CD350C3E2D15FC29F54'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
