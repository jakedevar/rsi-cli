# Sourced helper: keep the simulator's own Family service from crashing (#509).
#
# On the iOS 26.5 runtime Family.app (com.apple.family) is started on demand by
# familynotificationd and sometimes aborts in dyld init (an NSAssertionHandler
# failure in +[SpringBoardUI load]). macOS' ReportCrash then raises a "Family
# quit unexpectedly" dialog on the host. It is a simulator-runtime defect, not
# RSI code, and RSI tests never use Family notifications.
#
# sim_quiet_family <udid> boots the throwaway simulator <udid> (we created it),
# disables familynotificationd in that simulator's own launchd and boots out a
# running instance. The disable lives in the clone's data directory, so it
# survives later boots of the same clone and never touches the host's
# com.apple.CrashReporter settings; crashes of any other process, simulator or
# host, still report normally. Best effort: it returns 0 even when a step fails
# and the run continues exactly as before (a pre-boot disabled.plist is ignored
# by the runtime, so the simulator has to be booted first).
sim_quiet_family() {
    local udid="${1:-}"
    [ -n "$udid" ] || return 0
    xcrun simctl boot "$udid" > /dev/null 2>&1 || true
    xcrun simctl bootstatus "$udid" -b > /dev/null 2>&1 || true
    xcrun simctl spawn "$udid" launchctl disable user/foreground/com.apple.familynotification \
        > /dev/null 2>&1 || true
    xcrun simctl spawn "$udid" launchctl bootout user/foreground/com.apple.familynotification \
        > /dev/null 2>&1 || true
    return 0
}
