# zesterm's ZDOTDIR shim. Generated -- edits here are overwritten at spawn.
# Login shells only. See .zshenv.

ZESTERM_ZDOTDIR=$ZDOTDIR
ZDOTDIR=${ZESTERM_USER_ZDOTDIR:-$HOME}
[[ -f $ZDOTDIR/.zprofile ]] && source $ZDOTDIR/.zprofile
ZESTERM_USER_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$ZESTERM_ZDOTDIR
