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