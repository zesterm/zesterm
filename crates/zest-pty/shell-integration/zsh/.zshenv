# zesterm's ZDOTDIR shim. Generated -- edits here are overwritten at spawn.
#
# zsh always sources $ZDOTDIR/.zshenv first, which is the only reason this
# mechanism works: point ZDOTDIR at a directory of ours and zsh loads our files
# instead of the user's. The entire job of these shims is to hand control back
# so the user's own dotfiles run exactly as they would have, in the same order.
#
# **No file of the user's is written or modified.** That is the whole reason
# this approach was chosen over appending a line to ~/.zshrc.

ZESTERM_ZDOTDIR=$ZDOTDIR
ZDOTDIR=${ZESTERM_USER_ZDOTDIR:-$HOME}
[[ -f $ZDOTDIR/.zshenv ]] && source $ZDOTDIR/.zshenv

# The user's .zshenv is entitled to set ZDOTDIR itself -- that is the documented
# way to relocate zsh's config, and a framework may well do it. Honour wherever
# it ended up, then point back at ours so zsh finds our .zshrc next.
ZESTERM_USER_ZDOTDIR=$ZDOTDIR
ZDOTDIR=$ZESTERM_ZDOTDIR
