# zesterm shell integration for PowerShell.
#
# Emits the OSC 133 semantic prompt markers that turn scrollback into command
# blocks, plus the working directory. Loaded automatically when zesterm spawns
# the shell (see `shell_integration.rs`), or by hand:
#
#     zesterm --shell-integration pwsh | Out-String | Invoke-Expression
#
# Safe to load twice; every step below is guarded.
#
# PowerShell has no ZDOTDIR, so this is dot-sourced from the spawn command line
# rather than loaded by a shim the shell finds on its own. That is the only seam
# it has, and it has one advantage over the zsh path: dot-sourcing is
# independent of profiles, so integration works even for someone who runs
# `pwsh -NoProfile`.

# Already loaded. A nested pwsh inherits nothing, but a hand-written
# `Invoke-Expression` in a profile could run this twice, and doubling the hooks
# would emit every marker twice -- which reads to the parser as an empty block
# between each real one.
if ($global:__zesterm_loaded) { return }
$global:__zesterm_loaded = $true

# PSReadLine is not optional here, and that is a deliberate refusal rather than
# an oversight. `PSConsoleHostReadLine` is the only preexec PowerShell has, so
# without it there is no `C` marker -- and A/B/D without C produces blocks that
# start and never finish, which renders worse than no blocks at all.
if (-not (Get-Module -Name PSReadLine)) {
    return
}

$esc = [char]27
function global:__zesterm_osc([string]$body) {
    [Console]::Write("$esc]$body" + [char]7)
}

# Whether a command is actually running, so the *first* prompt of a session does
# not report the exit status of something that never ran. Keyed on the history
# id rather than a flag the way zsh keys on `__zesterm_running`: Ctrl+C at an
# empty prompt adds no history entry, so it correctly reports nothing.
$global:__zesterm_last_history = -1

# `prompt` is PowerShell's `precmd`, and the only place `A` and `B` can go.
#
# Printed from anywhere else, `A` lands on the line *before* the prompt and `B`
# cannot be placed at all -- the same reason the zsh hook wraps PS1 rather than
# printing from precmd. PSReadLine re-invokes `prompt` on every redraw, so the
# markers survive a resize and a history recall.
$global:__zesterm_inner_prompt = $function:prompt

function global:prompt {
    # FIRST, before anything can clobber them. `$?` is the success of the last
    # statement, and merely *reading* it into a variable is itself a statement --
    # so the order of these two lines is load-bearing, exactly as `local ret=$?`
    # is in the zsh hook.
    $ok = $?
    $code = $global:LASTEXITCODE

    $id = (Get-History -Count 1 | Select-Object -Last 1).Id
    if ($null -ne $id -and $id -ne $global:__zesterm_last_history) {
        $global:__zesterm_last_history = $id
        # `[int]!$?` -- what most integrations emit -- reports every failure as
        # 1, which loses the exit code of `cmd /c exit 3`. Prefer the real code
        # when there is one.
        $status = if ($ok) { 0 } elseif ($code) { $code } else { 1 }
        __zesterm_osc "133;D;$status"
    }

    # The working directory, as a native path. OSC 7 wants a file:// URL and its
    # Windows spelling is a pit -- `file://HOST/C:/x` parses back to `/C:/x` in
    # most consumers -- so the cwd goes over OSC 633 instead, which takes the
    # path as-is. OSC 7 is emitted too, for anyone eval'ing this hook in another
    # terminal, and second so ours wins.
    $cwd = (Get-Location).Path
    __zesterm_osc "633;P;Cwd=$cwd"

    $inner = & $global:__zesterm_inner_prompt
    # A and B bracket the prompt: A where it begins, B where the typed command
    # does. Unlike zsh there is no `%{...%}` to say "these bytes are zero-width";
    # PSReadLine measures the rendered prompt itself and does not count escapes.
    "$esc]133;A" + [char]7 + ($inner -join "") + "$esc]133;B" + [char]7
}

# `PSConsoleHostReadLine` is PowerShell's only preexec. It also hands us the
# command text explicitly, which is strictly better than the parser reading it
# back off the grid.
$global:__zesterm_inner_readline = $function:PSConsoleHostReadLine

function global:PSConsoleHostReadLine {
    $line = & $global:__zesterm_inner_readline
    if ($line) {
        __zesterm_osc "633;E;$line"
    }
    __zesterm_osc "133;C"
    $line
}
