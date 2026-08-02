# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-08-02
# after the owner-directed WSL performance work updated PLAN's WSL profile row
# (8 MiB/32 workers), striping locality, I/O-strategy row, and dispositioned
# hypothesis H7 with measured Plan 9 evidence (BENCHMARKS.md, ADR 0052)
# without changing VISION or LIMITATIONS.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'C6F5384A1A202726CF47E1FAF9D190A398A5A59697D7DA7365BF0F03BBF29F68'
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
