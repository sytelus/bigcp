# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-authorized second repository review documented disjoint audit
# roles, bounded journal replay, and exact standard-path memory accounting.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '4CA8ED4A2BD6696918CB6407FA87CBF5959F9C62B2AE58C0E8A3BFD8C102141D'
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
