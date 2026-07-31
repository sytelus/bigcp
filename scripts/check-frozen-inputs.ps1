# Verifies that the implementation task's three governing inputs remain
# byte-identical to their last owner-approved revision. Re-pinned 2026-07-30
# after the owner-authorized FAT/FAT32/exFAT scope and policy change updated
# VISION.md, PLAN.md, and LIMITATIONS.md together with ADR 0035.
# Re-pin only as the final step of an owner-approved documentation change.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = '19C964E77CF5363F36F8F664CE79341E6F94CBEDB86117AFF6203BB37DE0A427'
    'VISION.md' = 'FCB948161642B8519AED534C05183713180C4405B0A8DB20A1AB122A4F898C5B'
    'LIMITATIONS.md' = 'EAD3C7E22F2F760CA4AC213E82CBFDB34EDDC249092CD18669A0D507DBC1B72D'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Governing input hashes are unchanged.'
