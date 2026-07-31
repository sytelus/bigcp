# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-31
# after the owner-directed Recycle Bin default-inclusion and terminal UX
# changes updated PLAN and LIMITATIONS without changing VISION.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '21A67604B8E4DB99665FC7466C0D9C384F08510805553AF2831B6A4CEBD04D16'
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
