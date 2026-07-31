# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-authorized persistence review documented exact-handle state
# artifact publication; VISION and LIMITATIONS remained byte-identical.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'D7E808DB8B1DF688D96E59DBED4BF62452156A06416141053CC1A9AD54754B4A'
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
