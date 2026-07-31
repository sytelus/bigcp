# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-requested WSL throughput work added ADR 0046's distinct WSL
# transport/profile and synchronized PLAN/LIMITATIONS without changing VISION.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '669100F814733B53F06E5B805BE60F3B783F8049F242ECD9A813D80EA549F65F'
    'VISION.md' = 'B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28'
    'LIMITATIONS.md' = 'AAA4C00755C665EBB32FB6E6E3D77EB10A1F073EAEA08D9466F8972E05D3084B'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
