# Open the latest dev build, from anywhere.
#
#   zesterm-dev.ps1                 build and open, in the current directory
#   zesterm-dev.ps1 --no-build      open what is already built
#   zesterm-dev.ps1 --release       the shipping build instead of the fast one
#   zesterm-dev.ps1 --startup-probe anything else is passed straight through
#
# The Windows half of `scripts/zesterm-dev`. Same contract, different shell:
# Git Bash cannot run the sh one usefully here anyway -- MSYS rewrites paths
# that look unix-ish, which is exactly what a pipe path and a `--cmd` bound for
# another machine look like (AGENTS.md, #20).
#
# Two things this exists to get right, both of which have cost time already:
#
# 1. **It builds the whole workspace.** The window is a client of `zest-daemon`
#    (ADR-007) and finds it as a sibling of its own binary, so building only
#    `zest-app` leaves a new window talking to yesterday's daemon. That failure
#    is invisible -- everything connects and works, just not the code you
#    changed. Then `zest-mcp` repeated it from the other side: another client
#    of the same daemon, found at a fixed path by an MCP config, absent from
#    the list this script named -- so a run right after #349 landed opened a
#    current window against the previous day's MCP server, and the change
#    under test was the one thing not running (#358). Hence the workspace
#    rather than a list: a list only names the clients somebody remembered.
#
#    Windows write-locks a running binary, so this now fails outright when an
#    agent session is holding `zest-mcp.exe` open, rather than skipping it.
#    That is the trade taken deliberately -- a build that silently omits the
#    binary under test is the bug above, and `os error 5` names the file to
#    kill. Renaming the exe aside also frees the path; Windows permits that
#    while the process runs.
#
# 2. **A failed build never opens a window.** Launching the previous binary
#    after a compile error is how you spend ten minutes testing a change that
#    did not compile.
#
# `--profile fast` by default: ~3.6s to rebuild against ~51s for release (thin
# LTO, one codegen unit), and within a few percent at runtime, so startup and
# frame numbers measured on it still mean something.

$ErrorActionPreference = 'Stop'

# Where the repo is. The script lives in it; `$PSScriptRoot` is already the
# resolved directory of this file, so the sh version's symlink walk has no
# counterpart to write.
$repo = if ($env:ZESTERM_REPO) { $env:ZESTERM_REPO } else { (Resolve-Path (Join-Path $PSScriptRoot '..')).Path }

$profileName = 'fast'
$build = $true
$passthrough = @()
# Some flags make zesterm print something and exit rather than open a window.
# Those must stay in the foreground with stdout attached, or the answer goes
# nowhere along with the window that was never opened.
$foreground = $false

foreach ($arg in $args) {
    switch ($arg) {
        '--no-build' { $build = $false }
        '--release'  { $profileName = 'release' }
        '--debug'    { $profileName = 'dev' }
        { $_ -in '-h', '--help' } {
            # The usage block above is the only copy; printing it from here is
            # what keeps the two from drifting.
            Get-Content $PSCommandPath | Select-Object -Skip 2 -First 5 |
                ForEach-Object { $_ -replace '^# ?', '' }
            exit 0
        }
        { $_ -in '--startup-probe', '--attach-probe', '--themes', '--schema', '--shell-integration' } {
            $foreground = $true
            $passthrough += $arg
        }
        default { $passthrough += $arg }
    }
}

# `dev` is the one profile whose directory is not its name.
$dir = if ($profileName -eq 'dev') { 'debug' } else { $profileName }
$bin = Join-Path $repo "target\$dir\zesterm.exe"

if ($build) {
    # The workspace, and quietly: the point is a window, not a build log. A
    # failure still prints, because cargo writes errors to stderr.
    Push-Location $repo
    try {
        cargo build -q --profile $profileName --workspace
    } finally {
        Pop-Location
    }
    if ($LASTEXITCODE -ne 0) {
        Write-Error 'zesterm-dev: build failed; not opening a stale binary'
        exit 1
    }
}

if (-not (Test-Path $bin)) {
    Write-Error "zesterm-dev: nothing built at $bin`nzesterm-dev: run without --no-build, or check ZESTERM_REPO"
    exit 1
}

# Say which binary and how old, so "latest" is checkable rather than assumed.
# The daemon may well be older than the window and that is usually fine -- it is
# reused across launches by design -- but when it is not, this is where you see
# it.
$built = (Get-Item $bin).LastWriteTime.ToString('HH:mm:ss')
Write-Host "zesterm-dev: $dir build from $built"

# `Start-Process -ArgumentList` does not re-quote its elements, so an argument
# containing a space arrives as two. Quote here rather than hoping none does:
# `--font 'JetBrains Mono'` is the ordinary case that finds this.
$quoted = @($passthrough | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } })

if ($foreground) {
    # `-Wait`, because release builds are GUI-subsystem: the shell does not wait
    # for them, so `--themes` would return the prompt before printing a thing.
    # `-NoNewWindow` keeps this console, which is the one the binary attaches to.
    $p = if ($quoted.Count) {
        Start-Process -FilePath $bin -ArgumentList $quoted -NoNewWindow -Wait -PassThru
    } else {
        Start-Process -FilePath $bin -NoNewWindow -Wait -PassThru
    }
    exit $p.ExitCode
}

# Detached, so the shell comes straight back and closing this terminal does not
# take the window with it. The working directory is deliberately *not* the repo:
# the shell inside should start wherever you ran this.
if ($quoted.Count) {
    Start-Process -FilePath $bin -ArgumentList $quoted | Out-Null
} else {
    Start-Process -FilePath $bin | Out-Null
}
