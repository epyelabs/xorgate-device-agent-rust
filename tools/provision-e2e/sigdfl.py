#!/usr/bin/python3 -I
"""sigdfl.py <cmd> [args]: exec <cmd> with SIGINT/SIGHUP/SIGPIPE at their
defaults, so a test can signal a backgrounded "session" (bash starts async
children with SIGINT ignored, and an ignored signal cannot be trapped)."""
import os
import signal
import sys

for s in (signal.SIGINT, signal.SIGHUP, signal.SIGPIPE):
    signal.signal(s, signal.SIG_DFL)
os.execvp(sys.argv[1], sys.argv[1:])
