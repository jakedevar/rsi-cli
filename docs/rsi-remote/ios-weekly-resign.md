# iOS re-sign (Mac)

A free Apple Personal Team provisioning profile expires 7 days after install. A
launchd user agent (`dev.rsi.ios-resign`, hub #2963, retry policy hub #2979) keeps the
RSI Remote iOS app signed. It fires every 6 hours and `scripts/ios/weekly-resign.sh`
decides whether an install is due.

- Install: `scripts/ios/install-weekly-agent.sh [--dry-run] [--interval-hours 1-24]`
  (default every 6 h, plus once at load). Re-running replaces the agent. Remove with
  `--uninstall`.
- State: `~/.rsi/ios-resign.state` holds the epoch seconds of the last successful install.
  - Younger than 5 days: the run is a no-op and logs `skip` with the age.
  - From day 5 on, or with no (or an unreadable or future-dated) state file: it runs
    `scripts/ios/dev-install.sh --network-first` from the checkout the agent was installed from.
  - Success rewrites the state file (clock reset). Failure leaves it, so the next 6-hourly run
    retries until an install succeeds. The first agent run after install has no state and installs once.
  - Seed the state by hand to skip that: `date -u +%s > ~/.rsi/ios-resign.state`
    right after a manual install.
- Log: `~/.rsi/logs/ios-resign.log`, one line per run:
  `<RFC3339 UTC> ok|skip|failed exit=<n> <reason>`.
  - Exit 3 reads `failed exit=3 needs-phone ...` (no reachable, unlocked iPhone). The Mac PM greps
    it (`grep needs-phone ~/.rsi/logs/ios-resign.log`) and raises the to-do; the script never calls rsid.
  - Exit 4 reads `failed exit=4 needs-team ...`.
  - A failed run leaves its full output in `~/.rsi/logs/ios-resign.last.log` (removed again
    after a successful run; `skip` runs leave it alone).
- The plist names only the script path, `PATH` and `HOME`. Device id, team id and Apple ID
  come from env/local config read by `dev-install.sh` at run time and are never stored here.
- Scripts run on macOS `/bin/bash` 3.2 and Linux bash 5. Tests:
  `cd scripts && python3 -m unittest tests.test_ios_weekly_agent`. Env overrides:
  `RSI_IOS_RESIGN_STATE`, `RSI_IOS_RESIGN_NOW` (epoch), `RSI_IOS_RESIGN_DUE_DAYS`, `RSI_IOS_DEV_INSTALL`.
