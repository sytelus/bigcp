# Verifies that the implementation task's three frozen inputs remain byte-identical.
# Re-pinned 2026-07-29 after the owner removed /J from VISION.md's expressed
# defaults and the plan adopted buffered streaming as the final engine
# (ADR 0028). Re-pin only as the final step of an owner-approved doc change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '1EDF3A9EB20B7FF54E7F6C43F4FAF34EBAA571F8C29CF25363C09305DD49CCBC'
    'VISION.md' = 'C6446CDF4485E4D0D17118B34BBA1D0E44140FA45F674F6889ACD8374C417FDC'
    'LIMITATIONS.md' = 'AB0FFB20BDB0BBEA815DFBAFAEC8DAB4A44E8C6233F46E74FBCA641373A759C9'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
