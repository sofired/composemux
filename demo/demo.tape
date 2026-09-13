# The README recording, as code. `vhs demo.tape` from this directory, with the
# stack already up (`./stack up`) and a release composemux built.
#
# `record-drive` is still the event driver - it fires jobs at 22s, a traceback
# at 32s and a crash at 40s after it prints "go" - and this tape's sleeps are
# lined up against that schedule. The opening beat is long on purpose: it
# absorbs the second or two of slop between record-drive's "go" and composemux
# finishing its first draw.
#
# Beats, from #84: read the list, pin two services, traffic in both panes, a
# job drawing a progress bar, a stack trace, one status transition. The
# Screenshot lands after the last of those and is the README preview.

Output demo.gif

Set Shell bash
Set FontSize 18
Set Width 1280
Set Height 640
Set Padding 12
Set TypingSpeed 40ms
# Frame rate barely moves the GIF's size: the traffic loop scrolls both panes
# on nearly every frame, so size follows pixels changed, not frames. The real
# saving is the palette, which VHS cannot set from a tape - ./record does it.
Set Framerate 10

# Off camera: put the release binary on PATH, start the event driver, and
# launch composemux, so the take opens on the service list rather than on a
# prompt. The `-c` is demo plumbing - it picks the config with nothing pinned -
# and not the invocation a user types, so it does not belong on camera.
#
# record-drive runs a health check, counts down, and prints `go`, then schedules
# its events at go+22, go+32 and go+40. Wait for `go` rather than a fixed delay,
# so the tape's interaction clock and record-drive's event clock share one
# origin; a slow health check then shifts both together instead of sliding the
# events under the pins. Its output is left on screen (not /dev/null) only so
# `Wait` can read `go` -- the whole handshake is inside `Hide`, and `clear`
# wipes the countdown before composemux draws.
Hide
Type "export PATH=$PWD/../target/release:$PATH; ./record-drive &"
Enter
Wait+Screen@60s /go/
Type "clear"
Enter
Type "composemux -c .composemux.demo.yaml"
Enter
Sleep 1500ms
Show

# 0:00 - just the service list. The selection starts on api.
Sleep 6s

# Pin api to pane 2, walk down to gateway, pin it to pane 1. One request from
# record-drive's traffic loop now shows up in both panes.
Type "2"
Sleep 2s
Down 4
Sleep 500ms
Type "1"
# 9s, not 12: waiting for `go` launches composemux ~2.7s later than the old
# fixed delay did, so this sleep gives that back to keep the worker pinned just
# before record-drive's go+22 jobs event, catching the progress bar from 0%.
Sleep 9s

# ~0:22 - record-drive queues jobs. Swap worker into pane 2 to watch its
# progress bar draw live. Down 2, not 3: at this point only migrate has exited,
# so the sidebar is api, cache, db, flaky, gateway, metrics, worker (running,
# by name), then migrate (exited) last -- worker is two below gateway, and Down 3
# would land on migrate and replay its finished bar instead.
Down 2
Type "2"
Sleep 9s

# ~0:32 - the traceback lands in api. Swap it back into pane 2. Up 6, not 7:
# selection is on worker (index 6) after the beat above, the list wraps at the
# top, and a 7th Up would wrap past api round to migrate at the bottom. Up 6
# lands on api exactly. (This count is paired with the Down 2 above; changing
# one without the other pins the wrong service.)
Up 6
Type "2"
Sleep 17s

# ~0:45 - flaky has crashed and its row has gone red. Everything the beat sheet
# asks for is on screen at once, so this is the preview.
Screenshot preview.png
Sleep 3s

Type "q"
Sleep 1s
