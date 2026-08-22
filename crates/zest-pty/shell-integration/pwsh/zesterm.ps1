# zesterm shell integration for PowerShell.
#
# Emits the OSC 133 semantic prompt markers that turn scrollback into command
# blocks, plus the command line itself as OSC 633;E and the working directory as
# OSC 633;P. Loaded automatically when zesterm spawns the shell -- PowerShell has
# no ZDOTDIR analogue, so the command line zesterm builds dot-sources this file
# -- or by hand:
#
#     zesterm --shell-integration pwsh | Out-String | Invoke-Expression
#
# Windows PowerShell 5.1 is a supported host, so nothing newer than 5.1 syntax
# appears below: no ternary, no `??`, no `$IsWindows`, no `$PSStyle`. 5.1 is the
# fallback when PowerShell 7 is not installed, and a hook that only runs on 7 is
# a hook that is missing exactly where the user has least choice.
#
# Safe to load twice; every step below is guarded.

# Already loaded. Someone with the line above in their $PROFILE *and* a zesterm
# window loads this twice, and doubling the hooks emits every marker twice --
# which reads to the parser as an empty block between each real one.
if (Test-Path variable:global:__zesterm) { return }

# A constrained or restricted language mode cannot define the functions below.
# Failing loudly at every single prompt is worse than having no command blocks.
if ($ExecutionContext.SessionState.LanguageMode -ne 'FullLanguage') { return }

$Global:__zesterm = @{
    # Chained, never clobbered: whatever built the prompt before us -- the
    # user's own, oh-my-posh, starship, posh-git -- still builds it. A prompt
    # that silently loses its configuration to a terminal is the failure people
    # do not suspect the terminal for.
    OriginalPrompt   = $function:prompt
    OriginalReadLine = $null
    # Whether a command is actually running, so the *first* prompt of a session
    # does not report the exit status of something that never ran. A `D` with no
    # `C` before it is a block that finished without starting.
    Running          = $false
    # $LASTEXITCODE as it stood when the command was submitted. See the status
    # calculation in the prompt for why the *previous* value is the interesting
    # one.
    LastExit         = $null
    # Last-sent value per 633 fact property, so an unchanged prompt emits
    # nothing and a cleared value is sent exactly once.
    Facts            = @{}
}

# PowerShell has no `printf`, and `Write-Host` appends a newline. Returning the
# string lets the prompt accumulate one and the readline hook write one.
function Global:__zesterm_osc([string] $body) {
    "$([char]27)]$body$([char]7)"
}

# An OSC value must not carry the `;` that separates its fields, nor a control
# byte, nor the `\` that is the encoding's own escape. `\xNN` is VS Code's
# dialect, and zesterm's parser already undoes it for `633;E`.
function Global:__zesterm_escape([string] $value) {
    [regex]::Replace($value, "[$([char]0)-$([char]31)\\;]", {
            param($match)
            -join (
                [System.Text.Encoding]::UTF8.GetBytes($match.Value) |
                ForEach-Object { '\x{0:x2}' -f $_ }
            )
        })
}

# One 633 property, sent only when its value changed since last sent. An empty
# value is sent too -- once -- because a deactivated venv must take its chip
# with it; an empty value that was never non-empty sends nothing, because the
# hashtable's missing key and '' compare equal below on purpose.
function Global:__zesterm_fact([string] $key, [string] $value) {
    if ([string] $Global:__zesterm.Facts[$key] -eq $value) { return '' }
    $Global:__zesterm.Facts[$key] = $value
    __zesterm_osc "633;P;$key=$(__zesterm_escape $value)"
}

# PSReadLine is what knows a command was *submitted*, which is where `C` and the
# command line itself come from.
#
# Measured on PowerShell 7.6.4 under ConPTY, because the answer is not obvious:
# PSReadLine *is* already imported when `-Command` runs, so the call at the
# bottom of this file is the one that does the work. The hook is nevertheless
# re-attempted from the prompt, for the same reason the zsh hook re-wraps PS1 on
# every prompt rather than once -- it costs a hashtable lookup, it covers a host
# that imports PSReadLine later or not at all at load, and being wrong about it
# means no command blocks with nothing on screen to say so.
function Global:__zesterm_hook_readline {
    if ($Global:__zesterm.OriginalReadLine) { return }
    if (-not (Get-Module -Name PSReadLine)) { return }

    $Global:__zesterm.OriginalReadLine = $function:PSConsoleHostReadLine
    Set-Item -Path function:global:PSConsoleHostReadLine -Value {
        $line = $Global:__zesterm.OriginalReadLine.Invoke()
        $Global:__zesterm.Running = $true
        $Global:__zesterm.LastExit = $global:LASTEXITCODE

        # Written to the console rather than returned: this function's return
        # value *is* the command line the host is about to run.
        #
        # `633;E` states the command explicitly. zesterm can also read it back
        # off the grid between `B` and `C`, but that recovers what the screen
        # showed -- a recalled, edited or multi-line command is not the same
        # string as the one that runs.
        [Console]::Write(
            (__zesterm_osc "633;E;$(__zesterm_escape $line)") +
            (__zesterm_osc '133;C'))

        $line
    }
}

function Global:prompt {
    # `$?` and `$LASTEXITCODE` first, before anything else in this function can
    # clobber them -- the same reason zsh's precmd opens with `local ret=$?`.
    # Read later, they report the status of our own bookkeeping.
    $ok = $?
    $code = $global:LASTEXITCODE

    # For this scope only. Strict mode throws on the null comparisons below,
    # which is unhelpful rather than protective inside a prompt function.
    Set-StrictMode -Off

    $out = ''

    if ($Global:__zesterm.Running) {
        $Global:__zesterm.Running = $false

        # Two sources disagree, so ask which one is fresh. A native command sets
        # $LASTEXITCODE; a cmdlet or a parse error moves only `$?` and leaves
        # $LASTEXITCODE at whatever a native command set some commands ago.
        # Reporting that stale code as this command's status is worse than
        # reporting none.
        $status = 0
        if ($code -ne $Global:__zesterm.LastExit) {
            $status = $code
        }
        elseif (-not $ok) {
            # Only `$?` knows, and it knows only pass or fail.
            $status = 1
        }
        # Known corner, on 5.1 only: a native command that fails with the *same*
        # code twice running leaves $LASTEXITCODE unchanged, and 5.1 -- unlike
        # 7, where `$?` follows a native exit code -- keeps `$?` true. That
        # reports 0 for the second failure. Everything else is exact.
        $out += __zesterm_osc "133;D;$status"
    }

    # Before `A`, not after: the marker opens the block, and zesterm stamps the
    # working directory it knows *at that moment* onto it. Emitted afterwards,
    # every cwd would land one block late.
    #
    # `633;P;Cwd=` takes a plain path, where zsh's OSC 7 takes a `file://` URL.
    # That is not a preference: the parser strips the URL's host up to the first
    # `/`, so `file://box/C:/Users/andy` arrives as `/C:/Users/andy`.
    #
    # Only a real filesystem location is worth reporting: `$pwd` inside `HKLM:\`
    # or a `Function:` drive is not somewhere anyone can cd to.
    if ($pwd.Provider.Name -eq 'FileSystem') {
        $out += __zesterm_osc "633;P;Cwd=$(__zesterm_escape $pwd.ProviderPath)"
    }

    # Environment facts the terminal cannot see on its own: the child's env is
    # frozen at spawn on the daemon's side, so an activated venv exists only in
    # here. Each is sent only when it changed. No NvmBin arm -- `$NVM_BIN` is
    # unix nvm's export; nvm-windows sets nothing comparable.
    $venv = ''
    if ($env:VIRTUAL_ENV) { $venv = Split-Path -Leaf $env:VIRTUAL_ENV }
    $out += __zesterm_fact 'Venv' $venv
    $out += __zesterm_fact 'Conda' ([string] $env:CONDA_DEFAULT_ENV)
    $out += __zesterm_fact 'AwsProfile' ([string] $env:AWS_PROFILE)

    # Compact mode's blank row comes before `A`, so the block anchors on the
    # `❯` row and the line above is the chip layout's preferred home.
    if ($env:ZESTERM_COMPACT_PS1) { $out += "`n" }
    $out += __zesterm_osc '133;A'

    # Put `$?` and $LASTEXITCODE back exactly as the command left them. A prompt
    # that colours itself by success -- most frameworks do -- otherwise reports
    # on our bookkeeping instead of on the user's command.
    $global:LASTEXITCODE = $code
    if (-not $ok) { Write-Error 'zesterm' -ErrorAction Ignore }

    # Compact mode (`prompt.compact_ps1`): the chips are the prompt, so the
    # chained prompt is skipped for a bare `❯ `. Unlike the posix hooks
    # there is no framework check here: oh-my-posh installs by *replacing*
    # `function prompt`, which is the function this one chained at load —
    # OriginalPrompt IS the framework, and not invoking it is precisely what
    # the setting asks for.
    if ($env:ZESTERM_COMPACT_PS1) {
        $out += '❯ '
    }
    else {
        # `-join` because a prompt function may emit several strings; the
        # host concatenates them and so must we.
        $out += -join $Global:__zesterm.OriginalPrompt.Invoke()
    }

    # `B` marks where the *typed* command begins, so it belongs at the very end
    # of the prompt string rather than anywhere else.
    $out += __zesterm_osc '133;B'

    # Cheap, and it is what makes the hook survive PSReadLine arriving late.
    __zesterm_hook_readline

    $out
}

# The host may have PSReadLine already; if it does not, the first prompt will
# pick it up.
__zesterm_hook_readline
