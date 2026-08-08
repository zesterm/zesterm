# zesterm's ZDOTDIR shim. Generated -- edits here are overwritten at spawn.
#
# Login shells, after .zshrc. Reached only when .zshrc did not run -- a
# non-interactive login shell -- because .zshrc hands ZDOTDIR back to the user
# and zsh then finds their .zlogin directly. Present for that case rather than
# because it is the common one. See .zshenv.

ZESTERM_ZDOTDIR=$ZDOTDIR
ZDOTDIR=${ZESTERM_USER_ZDOTDIR:-$HOME}
[[ -f $ZDOTDIR/.zlogin ]] && source $ZDOTDIR/.zlogin
