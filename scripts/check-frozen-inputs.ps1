# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-08-02
# after the owner-directed consistency review reconciled PLAN sections
# 4.3/5.7/5.8/5.9/8.3/13.2 with ADRs 0052-0055 (they contradicted the
# already-updated sections) and brought LIMITATIONS' WSL/UNC performance
# statements current with the measured 2026-08-02 evidence (32-worker WSL
# row, either-side striping, segmented transfers, loopback-indicative UNC).
# VISION is unchanged.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'F9FC33C27C1A4A2A5627FDDCA98450C24831B35519EB2527DDE466F4FED29AC3'
    'VISION.md' = 'B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28'
    'LIMITATIONS.md' = '6EEA5B3B1C679E3FACFE4FD8B180FA38654873E3B9485B6A5C3E7CFEA518EA02'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
