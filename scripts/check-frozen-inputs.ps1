# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-30
# after the owner-authorized UNC/WSL endpoint change updated VISION.md,
# PLAN.md, and LIMITATIONS.md together with ADR 0037.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'BCEBCB7216E53F8FE5EE08991F81C1AE4EEFC6D38FA9879593096C36C7EFF799'
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
