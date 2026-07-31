# Verifies that the implementation task's three frozen inputs remain byte-identical.
# Re-pinned 2026-07-30 after the owner-authorized full-repository review
# reconciled PLAN.md and LIMITATIONS.md with the shipped plain-direct and
# transactional completion protocols plus the remaining pre-1.0 scaling gap,
# and again after the owner-approved review fixes (same-handle-only direct
# revalidation wording and the section 6.2 pseudocode variable fix).
# VISION.md is unchanged.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'BAA627C4626D6C7B793E327E9E085495D20209B6183800B1C014D0DA75899C86'
    'VISION.md' = 'C6446CDF4485E4D0D17118B34BBA1D0E44140FA45F674F6889ACD8374C417FDC'
    'LIMITATIONS.md' = 'DA26CD222BECEF0B6ED4F175FB6298ECD96C0D5FE60062DAF4926BAFBC426863'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
