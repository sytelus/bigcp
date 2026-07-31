# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-requested repository review reconciled audit, journal-replay,
# and active-design documentation without changing VISION or LIMITATIONS.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '725DE29541762D28FB0D96934B7A7972FF2EF8576B46667C4A630B240E840818'
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
