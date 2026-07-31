# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-authorized consistency review aligned the audit-failure
# contract with ADR 0027 and documented its user-visible limitation. The
# review did not edit VISION.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '32841B5B7B1742F9ABB89A09B93EE820B4B861572DC3FBDDC855F82215F766F0'
    'VISION.md' = 'B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28'
    'LIMITATIONS.md' = 'FD2F2C27F3C681C9BD54F469C5F73162B896EC4D2C8C1DFD5153B7E921A59123'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
