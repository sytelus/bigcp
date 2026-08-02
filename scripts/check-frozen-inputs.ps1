# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-08-02
# after the owner-directed NTFS performance work updated PLAN's I/O-strategy
# row (standard large streams now overlap through the two-buffer pipeline),
# profile table (NVMe 16 MiB), composition rules (MTL clamp removed, two
# coordinator chunks), and VMD device-bus fallback (BENCHMARKS.md, ADR 0055)
# without changing VISION or LIMITATIONS.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '10ADCA10F8FB682E2F11A86D592AEFC22F62442270720C240E7C39A35AE3D1CB'
    'VISION.md' = 'B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28'
    'LIMITATIONS.md' = 'E31391A9982F22DC74C0754587DBBE9B5066EBDB85294391486539AEDDEAABEE'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
