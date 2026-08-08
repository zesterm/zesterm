# zesterm's ZDOTDIR shim. Generated -- edits here are overwritten at spawn.
# Interactive shells. See .zshenv.

ZESTERM_ZDOTDIR=$ZDOTDIR
ZDOTDIR=${ZESTERM_USER_ZDOTDIR:-$HOME}
[[ -f $ZDOTDIR/.zshrc ]] && source $ZDOTDIR/.zshrc

# **After** the user's rc, never before. `add-zsh-hook` appends, so loading here
# puts our precmd last -- which is what lets it re-wrap a PS1 that a prompt
# framework rebuilds in its own precmd. Loading first would put us ahead of the
# framework and our wrapper would be overwritten on every prompt.
source $ZESTERM_ZDOTDIR/zesterm.zsh

# ZDOTDIR is deliberately left as the user's from here on. A nested `zsh`, a
# `su`, or anything else started from this shell should be an ordinary zsh --
# injection reaches the first shell only, by design, and pretending otherwise
# would mean a shim directory that outlives the process that made it.
ZDOTDIR=${ZESTERM_USER_ZDOTDIR:-$HOME}
