# zesterm shell integration for bash.
#
# Emits the OSC 133 semantic prompt markers that turn scrollback into command
# blocks, plus OSC 7 for the working directory. Loaded automatically when
# zesterm spawns the shell (see zesterm-shim.bash beside this file), or by hand:
#
#     eval "$(zesterm --shell-integration bash)"
#
# Safe to source twice; every step below is guarded. bash 3.2 syntax
# throughout, so macOS's /bin/bash (Apple's 3.2.57) can load it -- the one
# newer feature, PS0, is version-gated below with a DEBUG trap standing in
# where it is missing.

# Already loaded. A nested `bash` inherits the functions but re-sources this
# through a hand-written `eval` in the user's rc, and doubling the hooks would
# emit every marker twice -- which reads to the parser as an empty block
# between each real one.
[ -n "${__zesterm_loaded-}" ] && return
__zesterm_loaded=1

# Interactive shells only. `--init-file` is only read by interactive shells, so
# injection cannot get this wrong -- but the hand-loaded path can, and a DEBUG
# trap in a script would fire on every line of it.
case $- in *i*) ;; *) return ;; esac

# Whether a command is actually running, so the *first* prompt of a session does
# not report the exit status of something that never ran. A `D` with no `C`
# before it is a block that finished without starting.
__zesterm_running=

# `%s` rather than the sequence in the format string, so a `%` in a payload --
# a percent-encoded path -- is printed, not interpreted.
__zesterm_osc() { printf '\033]%s\007' "$1"; }

# The user's PROMPT_COMMAND, saved at load so __zesterm_precmd can run it and
# then re-wrap PS1 *after* a prompt framework rebuilt it. bash 5.1 made the
# variable an array; joined with `;` here because a string is what 5.0 and
# every hook-runner below understand.
if [ "${#PROMPT_COMMAND[@]}" -gt 1 ]; then
    __zesterm_user_prompt_command=$(IFS=';'; printf '%s' "${PROMPT_COMMAND[*]}")
else
    __zesterm_user_prompt_command=${PROMPT_COMMAND-}
fi

# `A` marks where the prompt begins and `B` where the typed command does, so
# they belong *inside* PS1 rather than in precmd: printed from precmd, `A` would
# land on the line before the prompt, and `B` could not be placed at all.
#
# `\[...\]` tells bash the enclosed bytes occupy no columns. Without it bash
# believes the prompt is wider than it is and mis-positions the cursor on every
# redraw -- which looks like a rendering bug and is not one.
__zesterm_wrap_prompt() {
    case $PS1 in *'133;A'*) return ;; esac
    PS1='\[\e]133;A\a\]'$PS1'\[\e]133;B\a\]'
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

    # OSC 7. The path is percent-encoded because a directory with a space in it
    # is otherwise a URL that stops at the space -- and `~/My Code` is exactly
    # the case that finds this.
    local encoded=${PWD//\%/%25}
    encoded=${encoded// /%20}
    encoded=${encoded//$'\n'/%0A}
    __zesterm_osc "7;file://${HOSTNAME-}${encoded}"

    # The user's own PROMPT_COMMAND, with the exit status it expects to read.
    # starship and oh-my-bash both live here and both consult `$?`; run with
    # ours they would render every prompt as a success.
    if [ -n "$__zesterm_user_prompt_command" ]; then
        (exit "$ret")
        eval "$__zesterm_user_prompt_command"
    fi

    # Re-wrap on every prompt, not once at load. A prompt framework rebuilds
    # PS1 in its own precmd just above, and a wrapper applied once at startup
    # is gone by the first prompt.
    __zesterm_wrap_prompt

    # Last, so the DEBUG trap ignores everything this function runs -- only
    # what bash executes *after* the prompt is a command worth marking.
    __zesterm_at_prompt=1
}

# preexec, in two dialects because the obvious one has a hole. A DEBUG trap
# fires before every *simple* command -- and not before a top-level compound:
# `(exit 3)` or `{ make; }` runs, finishes, and leaves no marker at all, which
# reads as a command the terminal never saw. PS0 has no such hole: it is
# expanded once per command *line* read, compounds included, and never for an
# empty Enter. PS0 needs bash 4.4, so the trap remains as the older bashes'
# stand-in (macOS's 3.2 reaches here), taking the hole over nothing at all.
if shopt -q promptvars && {
    [ "${BASH_VERSINFO[0]}" -gt 4 ] ||
        { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }
}; then
    # The subscript is the whole trick: prompt-string arithmetic runs in the
    # parent shell, so it can set __zesterm_running where a `$(...)` -- a
    # subshell -- cannot. The expansion itself (an unset array element, with a
    # `-` default so `set -u` has nothing to object to) adds nothing to the
    # output. No `\[ \]` here: outside PS1 nothing strips the \001/\002 they
    # decode to, and they would land in the output as bytes. And none of it
    # without promptvars, which is what makes `${...}` in a prompt string
    # expansion rather than literal text.
    PS0='\e]133;C\a${__zesterm_hush[$((__zesterm_running=1))]-}'
else
    __zesterm_debug() {
        # Programmable completion runs commands while the user is still typing.
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

    # Chain a pre-existing DEBUG trap rather than replacing it: `trap -p DEBUG`
    # prints a reusable `trap -- '<handler>' DEBUG` line, and eval-ing it into
    # `set --` recovers the handler word intact, quoting and all. Inside a
    # function, where `set --` rebinds the function's own positional parameters
    # rather than the shell's.
    __zesterm_install_debug_trap() {
        eval "set -- $(trap -p DEBUG)"
        if [ -n "${3-}" ]; then
            trap "__zesterm_debug; ${3-}" DEBUG
        else
            trap '__zesterm_debug' DEBUG
        fi
    }
    __zesterm_install_debug_trap
fi

# Ours runs after whatever the user's rc put in PROMPT_COMMAND (saved above),
# not beside it -- __zesterm_precmd is the sole entry so the ordering inside it
# is guaranteed: status first, their prompt rebuilt, our wrapper last.
PROMPT_COMMAND=__zesterm_precmd

# bash runs PROMPT_COMMAND before every primary prompt, the first included, so
# unlike the zsh hook nothing special is needed for the session's first block.
# Wrapping here too means a hand-loaded `eval` takes effect at the prompt it
# was typed at rather than one later.
__zesterm_wrap_prompt
