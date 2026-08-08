# zesterm shell integration for zsh.
#
# Emits the OSC 133 semantic prompt markers that turn scrollback into command
# blocks, plus OSC 7 for the working directory. Loaded automatically when
# zesterm spawns the shell (see the ZDOTDIR shim beside this file), or by hand:
#
#     eval "$(zesterm --shell-integration zsh)"
#
# Safe to source twice; every step below is guarded.

# Already loaded. A nested `zsh` inherits the functions but re-sources this
# through a hand-written `eval` in the user's rc, and doubling the hooks would
# emit every marker twice -- which reads to the parser as an empty block
# between each real one.
(( ${+__zesterm_loaded} )) && return
typeset -g __zesterm_loaded=1

autoload -Uz add-zsh-hook

# Whether a command is actually running, so the *first* prompt of a session does
# not report the exit status of something that never ran. A `D` with no `C`
# before it is a block that finished without starting.
typeset -g __zesterm_running=

# `print -n` rather than `printf` or `echo`: it is a builtin in every zsh, takes
# no format string to be confused by a `%` in a path, and does not fork.
__zesterm_osc() { print -n "\e]$1\a" }

__zesterm_precmd() {
    # First, and before anything else can clobber it: the status of whatever
    # just ran. A shell that reports no status at all is fine -- the terminal
    # treats that as unknown rather than success -- but reporting the *wrong*
    # status is not, and that is what reading `$?` later would do.
    local ret=$?

    if [[ -n $__zesterm_running ]]; then
        __zesterm_osc "133;D;$ret"
        __zesterm_running=
    fi

    # OSC 7. The path is percent-encoded because a directory with a space in it
    # is otherwise a URL that stops at the space -- and `~/My Code` is exactly
    # the case that finds this.
    local encoded=${PWD//\%/%25}
    encoded=${encoded// /%20}
    encoded=${encoded//$'\n'/%0A}
    __zesterm_osc "7;file://${HOST}${encoded}"

    # Re-wrap on every prompt, not once at load. A prompt framework --
    # oh-my-posh, starship, powerlevel10k -- rebuilds PS1 in its own precmd, and
    # a wrapper applied once at startup is gone by the first prompt.
    __zesterm_wrap_prompt
}

__zesterm_preexec() {
    __zesterm_running=1
    __zesterm_osc "133;C"
}

# `A` marks where the prompt begins and `B` where the typed command does, so
# they belong *inside* PS1 rather than in precmd: printed from precmd, `A` would
# land on the line before the prompt, and `B` could not be placed at all.
#
# `%{...%}` tells zsh the enclosed bytes occupy no columns. Without it zsh
# believes the prompt is wider than it is and mis-positions the cursor on every
# redraw -- which looks like a rendering bug and is not one.
__zesterm_wrap_prompt() {
    [[ $PS1 == *'133;A'* ]] && return
    PS1=$'%{\e]133;A\a%}'$PS1$'%{\e]133;B\a%}'
}

# Appended, so they run *after* whatever the user's rc registered. A prompt
# framework's precmd must have finished rebuilding PS1 before we re-wrap it.
add-zsh-hook precmd __zesterm_precmd
add-zsh-hook preexec __zesterm_preexec

# The first prompt of the session happens before any precmd, so wrap it now as
# well or the very first command produces no block.
__zesterm_wrap_prompt
