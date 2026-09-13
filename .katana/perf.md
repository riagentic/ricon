# Performance
- app is optimized for speed, user experience and ui response time as reasonably as possible
- the app draws at most once per frame and handles every event already queued before the next draw — a burst of input (mouse motion under an app in any-motion mode, a large paste) costs one frame, never one render per event
- nothing on the render path may block: PTY writes, AI-agent/model detection and /proc-wide scans all run off the UI thread
- a shell producing output at full speed must not starve the UI of the terminal-parser lock
- input the app cannot act on (a forwarded mouse report) must not do work that only typed input needs

