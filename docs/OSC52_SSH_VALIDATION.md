# OSC 52 and SSH/tmux live validation

On Windows, source `1e162e2a2df7a0dab11edc7ec6b99b279f66e707`
(debug executable SHA-256
`4736dfceacecb70953b0a3f22b6653854692c822729db8f367e29f361093de37`)
was exercised against an isolated Alpine 3.22.2 VM using QEMU software
emulation, OpenSSH and tmux 3.5a. No host directories were shared and no
host elevation or virtualization settings were changed. The VM used a
temporary public-key-only SSH identity with a pinned host key.

## Clipboard

The real Linux process sent OSC 52 over SSH into a visible Bayan window.
The Microsoft console host was verified loaded from the pinned bundle.
Only synthetic clipboard text was used.

- Reads disabled: no clipboard reply.
- Reads enabled: a real query raised the permission dialog. Enter returned
  exactly the synthetic clipboard text to the requesting process.
- A second request required a new confirmation. Esc returned no reply.
- Ten thousand queries produced one responsive permission dialog; refusing
  returned no clipboard data.
- Remote writes while copy was off did not change the Windows clipboard.
  With copy enabled, the expected synthetic remote value was copied.
- The native console regression separately verified that a query travels
  out through ConPTY and the exact reply reaches the requesting console.

## Real transport loss and recovery

A localhost TCP proxy forwarded SSH into the VM. Its cut operation set
SO_LINGER to zero and closed both sockets, producing a real TCP reset.
Two cuts were recorded with one live connection each. Bayan showed the
closed SSH transport; the remote tmux session remained detached and alive.

The same tmux server (PID 1964), session `$0`, and workload process (PID
1965) survived. The workload counter advanced from 6 before the first cut,
to 85 detached, to 138 after reconnecting, and 455 after the second cut.
Reattaching did not restart the workload. Window resizing reached tmux:
102×25 to 240×54 client cells (240×53 workload pane).

The disposable Alpine client initially used the C locale; reconnecting
with `LANG=C.UTF-8 tmux -u attach-session` restored correct Arabic. The
UTF-8 run displayed `العربية تعمل` with the same workload PID.

## Scope

These are Windows live tests of OSC 52, SSH transport and tmux recovery,
not a Linux Bayan port. They do not establish NVDA, Win+H, all IME, image
layout, or elevated WSL safety. The default suite does not run the native
host round-trip probe: invoke it explicitly under a standard Windows token
with the pinned pair next to the test executable and `BAYAN_TEST_PYTHON`
set. CI explicitly runs the signed-file/pin/write-lock verifier, and saves
the debug app with its console pair and a source/hash identity file.

## Mixed-language corpus (stronger follow-up)

A one-line Arabic greeting was insufficient evidence. The follow-up uses
`tests/fixtures/mixed-arabic-english.txt`: 279 words across Arabic and
English paragraphs, Arabic diacritics, Latin/Arabic digits, dates, times,
signed values, currencies, operators, URLs, and Windows/POSIX paths with
Arabic components. The unmodified CI build `798f088` copied the full corpus
back exactly, including soft-wrapped lines. A real Bash/readline edit also
matched the intended text after inserting `!` inside an Arabic word and
`[X=9]` before final punctuation in a mixed input line.

That larger test exposed reversed path-component order in RTL prose and
misplaced date/numeric prefixes. The renderer now adds LTR isolates only
in its shaped view around recognized paths and punctuation-bearing ASCII
numeric tokens. Application-supplied direction controls take precedence.
Grid storage, clipboard/export text, colors, styles, and real-character
cursor/hit-test maps remain unchanged. Tests check actual glyph positions
for the observed cases, and character/cursor mappings across the complete
corpus. This is a targeted correction, not a claim that every ambiguous
plain-text expression or every shell editor has identical BiDi semantics.

The live follow-up on `606a621` verified the actual draw path at narrow and
maximized widths. Paths, dates and numeric prefixes displayed in their
intended order. The 279-word copy again matched the original exactly after
resizing, without injected direction controls. A second real Bash/readline
edit inserted `!` after the third Arabic letter and preserved the rest of
the mixed input exactly. The glyph regression calls `draw_line_bidi` and
reads its cached run, so a hit-test-only change cannot satisfy it.
