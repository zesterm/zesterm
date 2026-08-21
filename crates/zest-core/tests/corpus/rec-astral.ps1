# Astral-plane output for the corpus (#17). Codepoints are built with
# ConvertFromUtf32 so no literal emoji appears anywhere ConPTY might echo it —
# the fixture's span assertions want the output rows to be the only ones
# carrying U+1F4A9.
$PSStyle.OutputRendering = 'Ansi'
$e = [char]27
$grin   = [char]::ConvertFromUtf32(0x1F600)
$rocket = [char]::ConvertFromUtf32(0x1F680)
$star   = [char]::ConvertFromUtf32(0x1F31F)
$poo    = [char]::ConvertFromUtf32(0x1F4A9)
$cjk    = "$([char]0x4E16)$([char]0x754C)"
Write-Host "$grin$rocket tail"
Write-Host "$e[33m$star$e[0m mixed $cjk ascii"
Write-Host "a${poo}b${poo}c"
Start-Sleep -Milliseconds 400
