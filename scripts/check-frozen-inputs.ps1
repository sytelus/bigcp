# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. PLAN re-pinned
# 2026-08-02 (large-stream close-out, ADR 0057): the I/O-strategy row,
# 5.11, the 6.5 pseudo-code, I9, 8.2's composition paragraph, and the 14.2
# streaming row record the local three-buffer pipeline with its dedicated
# in-flight-hash stage and the three-chunk standard mem accounting (the
# 6.5/I9/14.2 rows had also drifted from ADR 0055's two-chunk standard
# pipeline and are reconciled by the same edit). Prior re-pin the same day:
# ADR 0056 lane rotation, widened ADR 0048 gate, reparse drain rule.
# LIMITATIONS and VISION are unchanged.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '8F2132D07912041798E67158436E08AA7B34ABC40ED5C416F1C81C616D2FC0D1'
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
