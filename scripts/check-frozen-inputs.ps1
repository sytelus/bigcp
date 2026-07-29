# Verifies that the implementation task's three frozen inputs remain byte-identical.
# Re-pinned 2026-07-29 after the owner removed /J from VISION.md's expressed
# defaults and the plan adopted buffered streaming as the final engine
# (ADR 0028). Re-pin only as the final step of an owner-approved doc change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'B754B50B9537AE4DFECF6A422DE468743EA6DC60227204B52CEE75E1C003A28A'
    'VISION.md' = 'C6446CDF4485E4D0D17118B34BBA1D0E44140FA45F674F6889ACD8374C417FDC'
    'LIMITATIONS.md' = '6F633A233E0E160DCD2E26A7056168BF407046B51B0AF303F0ADFEBDCD0538D3'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
