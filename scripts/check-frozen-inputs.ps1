# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-authorized repository review corrected PLAN.md's as-built
# journal format, compaction, retention, and same-spindle status.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '9A90209EAA863C600C9C55728B7A576BCB3004323FD4B03FAFBC51383401996E'
    'VISION.md' = 'D8FAD02510CD02192D7D17571C96FAFF9C7673BA4FFC6312705731E91D93EC6B'
    'LIMITATIONS.md' = '6860A8AF1DC21C3E6E825C2B75210234CD811E8D5BB007F532EA5F266ED7215D'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
