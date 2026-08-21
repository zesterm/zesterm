# Hard scrolling at a natural viewport (#17). Write-Host flushes per line and
# ConPTY answers per write, so each line tends to arrive as its own pty read —
# each read boundary is a delta in the replay, and a SCROLL op only exists per
# delta. The occasional sleep keeps that true even if buffering changes. 120
# lines at 80x24 is ~96 scrolled lines: hard scrolling, sized like the real
# case rather than bigger.
1..120 | ForEach-Object {
  Write-Host ("line {0:d3} the quick brown fox jumps over the lazy dog" -f $_)
  if ($_ % 10 -eq 0) { Start-Sleep -Milliseconds 15 }
}
Start-Sleep -Milliseconds 400
