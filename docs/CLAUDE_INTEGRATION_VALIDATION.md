# Claude integration over main

Base: main `6e3742c9691c7eb19344e4beb094339bcaaaa665`.
Integrated the original histories of #74 (`27c9d87`), #79 (`33fd3a9`),
and #71/#81 (`bbab7a3` / `7440d3a`) through merge commits. Existing main
OSC 52 consent, pinned console host, Arabic shaping/token isolation,
regex search, export, session colors, and elevated environment sanitation
remain present.

## Conflict resolutions and runtime findings

- Retained dynamic test-count documentation instead of old manual totals.
- Kept mandatory protected Git Bash startup, `-p`, correct option order,
  environment sanitation, and fail-closed script creation. Added Claude's
  completion filtering and HOME restoration even when prompt marks are off.
- Combined startup-injection tests with login/non-login HOME/completion
  checks. Removed a global HOME mutation from a new test; protected
  materialization now explicitly tests refusal under a standard token and
  success under a privileged token when CI supplies one.
- A live arrow test exposed that legacy encoding ignored APP_CURSOR.
  Plain arrows/Home/End now use application sequences in that mode;
  modified keys and kitty protocol retain precedence. Reference:
  https://learn.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences#cursor-keys
- Arabic UI moved `+` away from `1000+`. Numeric search counts now have
  presentation-only LTR isolation. The regression uses actual draw_chrome.
- Duplicate tab lost the starting directory when no OSC cwd report existed.
  SessionMeta retains the actual initial directory as a separately checked
  fallback, without inventing a shell-originated report. Later cwd reports
  take precedence; remote/reparse paths remain refused.

## Live Windows checks

Code `cdc48e747a83d600bb2da84b0df95ad699b3dad3`, debug executable SHA-256
`929fd58cf741482f3aa519284391593cce9124fa622d833ada24487e6814b4ae`.
The app used an isolated home/config and the desktop_terminal_probe fixture;
input was recorded as data, never executed as shell commands.

- --maximized: IsZoomed true before automation activation.
- F11: 1100x700 window -> 2560x1440 full screen -> original window size.
- Search over 1200 old-history markers: 1000+ cap displayed correctly in
  Arabic; invalid regex and no-result feedback also visibly checked.
- Clear scrollback: exported old markers changed from 1200 to zero while
  the current screen and Arabic remained. A subsequent search found none.
- Reset modes: a real child received SS3 Up before reset and CSI Up after.
- Duplicate tab: distinct child processes, same shell and original folder,
  without any OSC cwd report. New window: a separate Bayan process.
- Mixed report: tests/fixtures/mixed-arabic-english.txt has 279 words,
  Arabic/English paragraphs, paths, dates, digits, signs and diacritics.
  Full keyboard selection/copy matched its 1789 non-trailing characters
  exactly after wrapping, without injected direction controls.

## Other gates and limitations

882 ordinary tests passed; six explicit native/measurement probes are not
part of the default run. Formatting, clippy with warnings denied, build,
and Rust 1.89 all-target check passed. Explicit signed-host verification,
real console OSC 52 round trip, and native mux probes also passed locally.

The mux probe uses the production server and two production clients, a
real Windows named pipe and ConPTY child, with a test-only pipe namespace.
The same workload PID survived detach/reattach; input/output and resize
were verified. It ran with inbox ConPTY and again with the pinned bundle.
Lua tests execute the actual Lua engine, including the generic-for closing
value attacks. Shell tests execute actual Git Bash, including planted files
and startup hooks; selected uid/mode metadata is controlled to exercise
Linux-only ownership branches. No live WSL distro, elevated Bash window,
or real Linux bash-completion package was exercised in this integration.
WSL root is not certified as a privilege boundary.
