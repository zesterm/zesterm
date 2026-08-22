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

    # Environment facts the terminal cannot see on its own: the child's env is
    # frozen at spawn on the daemon's side, so an activated venv exists only in
    # here. Parameter expansion and one builtin print per *changed* value --
    # never a fork, and an unchanged prompt emits nothing.
    __zesterm_fact Venv "${${VIRTUAL_ENV-}:t}" __zesterm_last_venv
    __zesterm_fact Conda "${CONDA_DEFAULT_ENV-}" __zesterm_last_conda
    __zesterm_fact AwsProfile "${AWS_PROFILE-}" __zesterm_last_aws
    __zesterm_fact NvmBin "${NVM_BIN-}" __zesterm_last_nvm

    # Re-wrap on every prompt, not once at load. A prompt framework --
    # oh-my-posh, starship, powerlevel10k -- rebuilds PS1 in its own precmd, and
    # a wrapper applied once at startup is gone by the first prompt.
    __zesterm_wrap_prompt
}

__zesterm_preexec() {
    __zesterm_running=1
    __zesterm_osc "133;C"
}

# One 633 property, sent only when its value changed since last sent. An
# empty value is sent too -- once -- because a deactivated venv must take its
# chip with it; an empty value that was *never* non-empty sends nothing,
# since an unset cache and an empty one compare equal on purpose. The value
# is escaped the way the 633 dialect asks (`\xNN`): `\` first or the escapes
# escape themselves, then `;` (the OSC field separator), then the control
# bytes that would end or garble the sequence.
__zesterm_fact() {
    local value=$2
    value=${value//\\/\\x5c}
    value=${value//;/\\x3b}
    value=${value//$'\e'/\\x1b}
    value=${value//$'\a'/\\x07}
    value=${value//$'\n'/\\x0a}
    if [[ ${(P)3-} != $value ]]; then
        typeset -g $3=$value
        __zesterm_osc "633;P;$1=$value"
    fi
}

# `A` marks where the prompt begins and `B` where the typed command does, so
# they belong *inside* PS1 rather than in precmd: printed from precmd, `A` would
# land on the line before the prompt, and `B` could not be placed at all.
#
# `%{...%}` tells zsh the enclosed bytes occupy no columns. Without it zsh
# believes the prompt is wider than it is and mis-positions the cursor on every
# redraw -- which looks like a rendering bug and is not one.
#
# Compact mode (ZESTERM_COMPACT_PS1, from the `prompt.compact_ps1` setting):
# the prompt *is* the chips, so PS1 collapses to a blank line -- the chips'
# guaranteed home -- and `❯ `. Only when nothing else owns PS1: a framework
# (p10k, starship, oh-my-posh) rebuilds it in its own precmd, which runs
# before this re-wrap, and clobbering their work is a fight the user picked
# a *framework* for, not this setting. The newline comes *before* the `A`
# marker on purpose: the block then anchors on the `❯` row, and the blank
# line above it is exactly the row the chip layout prefers -- left-aligned,
# the Warp shape -- rather than a wasted row inside the block.
__zesterm_wrap_prompt() {
    if [[ -n ${ZESTERM_COMPACT_PS1-} ]] && ! __zesterm_ps1_owned; then
        PS1=$'\n%{\e]133;A\a%}❯ %{\e]133;B\a%}'
        return
    fi
    [[ $PS1 == *'133;A'* ]] && return
    PS1=$'%{\e]133;A\a%}'$PS1$'%{\e]133;B\a%}'
}

# Whether a prompt framework owns PS1. Best effort, erring toward "owned":
# a framework we fail to detect overwrites our compact PS1 next precmd
# anyway, so a miss degrades to the framework winning -- the safe direction.
__zesterm_ps1_owned() {
    (( ${+functions[p10k]} )) && return 0
    [[ -n ${STARSHIP_SHELL-} || -n ${POSH_THEME-} ]]
}

# Appended, so they run *after* whatever the user's rc registered. A prompt
# framework's precmd must have finished rebuilding PS1 before we re-wrap it.
add-zsh-hook precmd __zesterm_precmd
add-zsh-hook preexec __zesterm_preexec

# The first prompt of the session happens before any precmd, so wrap it now as
# well or the very first command produces no block.
__zesterm_wrap_prompt
