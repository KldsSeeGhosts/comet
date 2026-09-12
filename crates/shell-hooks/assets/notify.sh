# No eval, background jobs, login shell or global shell configuration.
if [ "${1:-}" != '--drain' ]; then
    helper enqueue "$@" || exit 1
fi
# A competing drainer owns the queue. The host schedules subsequent bounded drains.
mkdir "$root/drain.lock" 2>/dev/null || exit 0
trap 'rmdir "$root/drain.lock" 2>/dev/null || :' EXIT
trap 'exit 1' HUP INT TERM
names=$(helper batch) || exit 1
for name in $names; do
    # batch emits only fixed-format UUID names; validate again at the shell boundary.
    case "$name" in *.json) stem=${name%.json} ;; *) continue ;; esac
    case "$stem" in *[!a-f0-9]*|'') continue ;; esac
    [ "${#stem}" -eq 32 ] || continue
    if helper deliver "$name" > /dev/null 2>&1; then
        helper ack "$name" || exit 1
    else
        helper fail "$name" || exit 1
    fi
done
