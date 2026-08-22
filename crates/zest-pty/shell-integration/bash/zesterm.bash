# zesterm shell integration for bash.
#
# Emits the OSC 133 semantic prompt markers that turn scrollback into command
# blocks, plus OSC 7 for the working directory. Loaded automatically when
# zesterm spawns the shell (see zesterm-shim.bash beside this file), or by hand:
#
#     eval "$(zesterm --shell-integration bash)"
#
# Safe to source twice; bash 3.2 syntax throughout, so macOS's /bin/bash
# (Apple's 3.2.57) can load it -- the one newer feature, PS0, is version-gated
# below with a DEBUG trap standing in where it is missing.
#
# One if/fi around the whole body rather than early `return`s: on the
# hand-load path this file arrives through `eval` inside someone's .bashrc,
# and a `return` there returns from *their rc*, silently skipping every line
# after the eval. The re-source guard and the interactive check must not cost
# the user the rest of their own configuration.
if [ -z "${__zesterm_loaded-}" ] && [[ $- == *i* ]]; then

# A nested `bash` inherits the functions but re-sources this through the
# user's rc, and doubling the hooks would emit every marker twice -- which
# reads to the parser as an empty block between each real one.
__zesterm_loaded=1

# Whether a command is actually running, so the *first* prompt of a session
# does not report the exit status of something that never ran. A `D` with no
# `C` before it is a block that finished without starting.
__zesterm_running=

# `%s` rather than the sequence in the format string, so a `%` in a payload --
# a percent-encoded path -- is printed, not interpreted.
__zesterm_osc() { printf '\033]%s\007' "$1"; }

# The user's PROMPT_COMMAND, separated from our own entry. Called at load, and
# again from precmd whenever something has edited the variable since -- a
# zoxide/direnv-style tool run at a prompt installs itself by prepending, and
# left in place it would sit between the command and our `$?` read, making
# every `D` carry the tool's status instead of the command's.
#
# The `+x` guard keeps `set -u` rcs loadable, and `${#...[@]}` handles the
# array PROMPT_COMMAND became in bash 5.1, joined because a string is what
# every older bash understands.
__zesterm_capture_prompt_command() {
    local joined
    if [ -n "${PROMPT_COMMAND+x}" ] && [ "${#PROMPT_COMMAND[@]}" -gt 1 ]; then
        joined=$(IFS=';'; printf '%s' "${PROMPT_COMMAND[*]}")
    else
        joined=${PROMPT_COMMAND-}
    fi
    # Drop our own entry, then the separators the removal strands.
    joined=${joined//__zesterm_precmd/}
    while case $joined in ';'*|' '*) true ;; *) false ;; esac; do
        joined=${joined#?}
    done
    while case $joined in *';'|*' ') true ;; *) false ;; esac; do
        joined=${joined%?}
    done
    while case $joined in *';;'*) true ;; *) false ;; esac; do
        joined=${joined//;;/;}
    done
    if [ -n "$joined" ]; then
        if [ -n "${__zesterm_user_prompt_command-}" ] && [ "$joined" != "${__zesterm_user_prompt_command-}" ]; then
            __zesterm_user_prompt_command="$__zesterm_user_prompt_command; $joined"
        else
            __zesterm_user_prompt_command=$joined
        fi
    fi
    PROMPT_COMMAND=__zesterm_precmd
}
__zesterm_user_prompt_command=
__zesterm_capture_prompt_command

# `A` marks where the prompt begins and `B` where the typed command does, so
# they belong *inside* PS1 rather than in precmd: printed from precmd, `A`
# would land on the line before the prompt, and `B` could not be placed at
# all.
#
# `\[...\]` tells bash the enclosed bytes occupy no columns. Without it bash
# believes the prompt is wider than it is and mis-positions the cursor on
# every redraw -- which looks like a rendering bug and is not one.
__zesterm_wrap_prompt() {
    case ${PS1-} in *'133;A'*) return ;; esac
    PS1='\[\e]133;A\a\]'${PS1-}'\[\e]133;B\a\]'
}

# One 633 property, sent only when its value changed since last sent. An
# empty value is sent too -- once -- because a deactivated venv must take its
# chip with it; an empty value that was *never* non-empty sends nothing,
# since an unset cache and an empty one compare equal on purpose. The value
# is escaped the way the 633 dialect asks (`\xNN`): `\` first or the escapes
# escape themselves, then `;` (the OSC field separator), then the control
# bytes that would end or garble the sequence. `${!name-}` and `printf -v`
# for the indirection -- both are bash 3.2 -- because the value comes from
# the environment, and an `eval` that is only safe by careful escaping is a
# review burden no prompt hook is worth.
__zesterm_fact() {
    local value=$2 cached
    value=${value//\\/\\x5c}
    value=${value//;/\\x3b}
    value=${value//$'\033'/\\x1b}
    value=${value//$'\007'/\\x07}
    value=${value//$'\n'/\\x0a}
    cached=${!3-}
    if [ "$cached" != "$value" ]; then
        printf -v "$3" '%s' "$value"
        __zesterm_osc "633;P;$1=$value"
    fi
}

__zesterm_precmd() {
    # First, and before anything else can clobber it: the status of whatever
    # just ran. A shell that reports no status at all is fine -- the terminal
    # treats that as unknown rather than success -- but reporting the *wrong*
    # status is not, and that is what reading `$?` later would do.
    local ret=$?

    if [ -n "$__zesterm_running" ]; then
        __zesterm_osc "133;D;$ret"
        __zesterm_running=
    fi

    # OSC 7. The path is percent-encoded because a directory with a space in
    # it is otherwise a URL that stops at the space -- and `~/My Code` is
    # exactly the case that finds this.
    local encoded=${PWD//\%/%25}
    encoded=${encoded// /%20}
    encoded=${encoded//$'\n'/%0A}
    __zesterm_osc "7;file://${HOSTNAME-}${encoded}"

    # Environment facts the terminal cannot see on its own: the child's env
    # is frozen at spawn on the daemon's side, so an activated venv exists
    # only in here. Parameter expansion and one builtin print per *changed*
    # value -- never a fork, and an unchanged prompt emits nothing.
    local venv=${VIRTUAL_ENV-}
    __zesterm_fact Venv "${venv##*/}" __zesterm_last_venv
    __zesterm_fact Conda "${CONDA_DEFAULT_ENV-}" __zesterm_last_conda
    __zesterm_fact AwsProfile "${AWS_PROFILE-}" __zesterm_last_aws
    __zesterm_fact NvmBin "${NVM_BIN-}" __zesterm_last_nvm

    # The user's own PROMPT_COMMAND, with the exit status it expects to read.
    # starship and oh-my-bash both live here and both consult `$?`; run with
    # ours they would render every prompt as a success.
    if [ -n "${__zesterm_user_prompt_command-}" ]; then
        (exit "$ret")
        eval "$__zesterm_user_prompt_command"
    fi

    # If something edited PROMPT_COMMAND since load, re-absorb it -- see
    # __zesterm_capture_prompt_command for who does that and what it costs.
    if [ "${PROMPT_COMMAND-}" != __zesterm_precmd ]; then
        __zesterm_capture_prompt_command
    fi

    # Re-wrap on every prompt, not once at load. A prompt framework rebuilds
    # PS1 in its own precmd just above, and a wrapper applied once at startup
    # is gone by the first prompt.
    __zesterm_wrap_prompt

    # Last, so the DEBUG-trap dialect ignores everything this function ran --
    # only what bash executes *after* the prompt is a command worth marking.
    __zesterm_at_prompt=1
}

# preexec, in two dialects because the obvious one has a hole. A DEBUG trap
# fires before every *simple* command -- and not before a top-level compound:
# `(exit 3)` or `{ make; }` runs, finishes, and leaves no marker at all, which
# reads as a command the terminal never saw. PS0 has no such hole: it is
# expanded once per command *line* read, compounds included, and never for an
# empty Enter. PS0 needs bash 4.4 and promptvars (on by default -- off, the
# `${...}` below would print as literal text), so the trap remains as the
# stand-in, taking the hole over nothing at all.
if shopt -q promptvars && {
    [ "${BASH_VERSINFO[0]}" -gt 4 ] ||
        { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }
}; then
    # The subscript is the whole trick: prompt-string arithmetic runs in the
    # parent shell, so it can set __zesterm_running where a `$(...)` -- a
    # subshell -- cannot. The expansion itself (an unset array element, with a
    # `-` default so `set -u` has nothing to object to) adds nothing to the
    # output. No `\[ \]` here: outside PS1 nothing strips the \001/\002 they
    # decode to, and they would land in the output as bytes. The user's own
    # PS0 stays, after ours -- it is their tooling, not ours to discard.
    case ${PS0-} in *'133;C'*) : ;; *)
        PS0='\e]133;C\a${__zesterm_hush[$((__zesterm_running=1))]-}'${PS0-}
    ;; esac
else
    __zesterm_debug() {
        # Programmable completion runs commands while the user is still
        # typing. (A `bind -x` widget -- fzf's Ctrl-R -- still slips through
        # on this dialect: bash 3.2 offers no marker for it, so a widget can
        # mint a phantom block. The PS0 dialect is immune; this one takes the
        # known hole over no hook at all.)
        [ -n "${COMP_LINE-}" ] && return
        [ -z "${__zesterm_at_prompt-}" ] && return
        # An empty Enter runs no command, but PROMPT_COMMAND itself trips the
        # trap on the way to the next prompt. That is a redraw, not a command
        # -- the same shape as zsh's bare-`A` prompts (#193).
        case $BASH_COMMAND in __zesterm_precmd*) return ;; esac
        __zesterm_at_prompt=
        __zesterm_running=1
        __zesterm_osc "133;C"
    }

    # Chain a pre-existing DEBUG trap rather than replacing it. The capture
    # happens *at the call*, in the argument: inside a function the DEBUG trap
    # is unset (bash does not inherit it without functrace), so `trap -p`
    # there reports nothing and the user's handler would be silently dropped.
    # As an argument it is read at top level, where a substitution consisting
    # solely of `trap` reports the shell's own traps. (bash 3.2 predates that
    # rule and loses the chain -- degraded, and better than the trap dying.)
    __zesterm_chain_debug() {
        eval "set -- ${1-}"
        if [ -n "${3-}" ] && [ "${3-}" != __zesterm_debug ]; then
            trap "__zesterm_debug; ${3-}" DEBUG
        else
            trap '__zesterm_debug' DEBUG
        fi
    }
    __zesterm_chain_debug "$(trap -p DEBUG)"
fi

# bash runs PROMPT_COMMAND before every primary prompt, the first included, so
# unlike the zsh hook nothing special is needed for the session's first block.
# Wrapping here too means a hand-loaded `eval` takes effect at the prompt it
# was typed at rather than one later.
__zesterm_wrap_prompt

fi # __zesterm_loaded / interactive
