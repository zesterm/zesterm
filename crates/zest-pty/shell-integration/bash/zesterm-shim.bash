# zesterm's --init-file shim. Generated -- edits here are overwritten at spawn.
#
# bash has no ZDOTDIR analogue; `--init-file` is the one per-invocation knob,
# and it *replaces* the interactive startup files rather than adding to them.
# So the first job here is to run exactly what bash would have run for an
# interactive non-login shell -- the system rc, then the user's -- and only
# then load the hook. (A login bash never reaches this file: injection
# declines `-l`/`--login`, because a login shell ignores `--init-file`.)
#
# **No file of the user's is written or modified.** That is the whole reason
# this approach was chosen over appending a line to ~/.bashrc.

[ -f /etc/bash.bashrc ] && . /etc/bash.bashrc
[ -f ~/.bashrc ] && . ~/.bashrc

# **After** the user's rc, never before. The hook saves PROMPT_COMMAND and
# takes the variable over; loaded first, a prompt framework's rc would clobber
# it a line later and there would be no blocks at all.
. "${BASH_SOURCE[0]%/*}/zesterm.bash"
