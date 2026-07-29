# Verifies that the implementation task's three frozen inputs remain byte-identical.
# Re-pinned 2026-07-29 after the owner removed /J from VISION.md's expressed
# defaults and the plan adopted buffered streaming as the final engine
# (ADR 0028). Re-pin only as the final step of an owner-approved doc change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'CB58D0FF840C1F11934628FD6DF6209A9AF8B45776414E136C732E6500B1BDB1'
    'VISION.md' = 'B54F1B7A6465599D3AD63B2897BCD411E9E2AAB223FAE7BFB3EF77BAA31F5393'
    'LIMITATIONS.md' = '6419240A16837F14B3D5B2B8A015D3A8B8FD9816423E0085A6EBB35660D598E0'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
