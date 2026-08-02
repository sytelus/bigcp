# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-08-02
# after the owner-directed UNC performance work extended PLAN's locality
# paragraph with the latency-gated generic-redirector source striping and
# annotated hypothesis H6 with loopback-indicative evidence (BENCHMARKS.md,
# ADR 0053) without changing VISION or LIMITATIONS.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'C52108E09C55625F08245BB4CA97B9E8D82E9EF7CDFD4FD9764C54FC690D36A7'
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
