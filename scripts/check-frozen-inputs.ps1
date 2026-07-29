# Verifies that the implementation task's three frozen inputs remain byte-identical.
# Re-pinned 2026-07-29 after the owner's VISION.md update (test prohibitions +
# live-run analysis), the corresponding review pass over PLAN.md and
# LIMITATIONS.md, and the owner-requested test-policy update (drive whitelist,
# two-tier tests). Re-pin only as the final step of an owner-approved doc change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'B56D924644546000C4A104B352DAA4B2BF7B2078D1E443B8304479431A7D3D07'
    'VISION.md' = '9D8EA73339F5358582457C1A3726817D1D138398E32740D72D00D7CCA422C2E1'
    'LIMITATIONS.md' = '28DD6E7C305F33FC109ABBA13C2E97E5DC27C1668094E78669D7548802E78A28'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
