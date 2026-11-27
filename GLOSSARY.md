# Bayan

Bayan (بيان) is an Arabic-first terminal emulator. This glossary names the
things a pane shows and the units they live in, so code, docs, and reviews
use one language.

## Language

**Session**:
One PTY and its terminal state: the shell process, the grid it writes, and
the shell-integration facts scanned from its output.
_Avoid_: terminal instance, shell session

**Pane**:
The on-screen region presenting one **Session**. The pane is what the user
looks at; the **Session** is what runs.
_Avoid_: panel, view

**Tab**:
A named group of **Panes** with one focused pane, laid out side by side or
stacked.
_Avoid_: window, workspace

**PaneFrame**:
An owned, immutable photograph of one **Pane**'s display state, taken once
per frame. Every consumer in that frame reads the same photograph.
_Avoid_: snapshot (alone), pane state, PaneSnap

**prepare**:
Assemble everything a shell launch needs — the resolved program, its
arguments, the generated environment, and the startup scripts — before the
PTY exists. One seam owns the security-critical ordering, so it is testable
as data without a console.
_Avoid_: spawn setup, launch protocol

**PreparedLaunch**:
The result of **prepare**: program, args, env, the session nonce, and the
armed script directory. Pure data plus one RAII guard; dropping it removes
the scripts, and on success the guard moves into the **Session**.
_Avoid_: launch plan, spawn args

**Policy**:
The caller's switches for **prepare**: integration enabled, elevated, and
the nonce (minted by the **Session** from the OS RNG). Tests inject a known
nonce to assert where it does and does not go.
_Avoid_: launch options, config