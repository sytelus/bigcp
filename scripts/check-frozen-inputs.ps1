# Verifies that the implementation task's three frozen inputs remain byte-identical.
$ErrorActionPreference = 'Stop'
$Expected = @{
    'PLAN.md' = 'E85B5AB9ABD335C9F277600416C296A320D35C2B41DB369A8E361E5E9B018C45'
    'VISION.md' = '1563557009A73096125F40BD0FFBB8C406E0F392D8FB121B147C46FDFBED99B8'
    'LIMITATIONS.md' = 'B66D610848E5BFD35ABD7C5B30EBF3E9311CFE393AF6563945F69BBF5673ECCE'
}

foreach ($entry in $Expected.GetEnumerator()) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $entry.Key).Hash
    if ($actual -ne $entry.Value) {
        throw "$($entry.Key) changed: expected $($entry.Value), found $actual"
    }
}

Write-Output 'Frozen input hashes are unchanged.'
