# Decomposed Unicode for the corpus (#17): explicit combining codepoints, the
# same shapes the synthetic combining-marks fixture used — a mark inside a
# coloured run, one after the run boundary, one riding bold+underline, and one
# on a double-width character.
$PSStyle.OutputRendering = 'Ansi'
$e = [char]27
$acute = [char]0x0301
$grave = [char]0x0300
$diaer = [char]0x0308
Write-Host "cafe$acute nai${diaer}ve"
Write-Host "$e[31mred e$acute$e[0m then a$grave"
Write-Host "$e[1;4mbold u$diaer$e[0m tail"
Write-Host "$e[32m$([char]0x4E16)$acute$([char]0x754C)$e[0m"
Start-Sleep -Milliseconds 400
