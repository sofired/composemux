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

# Off camera: put the release binary on PATH and start the event driver.
Hide
Type "export PATH=$PWD/../target/release:$PATH; ./record-drive >/dev/null 2>&1 & clear"
Enter
Sleep 500ms
Show

Type "composemux -c .composemux.demo.yaml"
Enter

# 0:00 - just the service list. The selection starts on api.
Sleep 6s

# Pin api to pane 2, walk down to gateway, pin it to pane 1. One request from
# record-drive's traffic loop now shows up in both panes.
Type "2"
Sleep 2s
Down 4
Sleep 500ms
Type "1"
Sleep 12s

# ~0:22 - record-drive queues jobs. Swap worker into pane 2 to watch the
# progress bar draw.
Down 3
Type "2"
Sleep 9s

# ~0:32 - the traceback lands in api. Swap it back into pane 2.
Up 7
Type "2"
Sleep 17s

# ~0:45 - flaky has crashed and its row has gone red. Everything the beat sheet
# asks for is on screen at once, so this is the preview.
Screenshot preview.png
Sleep 3s

Type "q"
Sleep 1s
