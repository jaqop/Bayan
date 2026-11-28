# Phase 2 — snippets (PR 2 of triggers/snippets)

**Date:** 2026-11-28 · **Status:** approved scope (owner: "both, triggers first")
**Companion:** output triggers (PR #130, merged).

## Goal

Saved, named text the user inserts into the focused pane from the command
palette — a lightweight snippet manager for frequent commands. No new
dependency; reuses the palette and the paste guard.

## Design (as implemented)

- **Config** (`config.json`): a `snippets` array of `{ "name": "...", "text": "..." }`.
  Default empty. Merged on config reload like every other field.
- **Palette**: a `Snippets` row (`PaletteRow::OpenSnippets`) opens the snippet
  list (`open_snippets_menu`), a palette sub-menu like the SSH host list
  (`Palette.snippet_menu`, rows in `extras`, filtered by the query). Each row is
  `PaletteRow::Snippet(text)` labelled by the snippet name (the text itself when
  the name is empty). With no snippets, a card explains how to add one.
- **Insertion**: selecting a snippet sets `pending_paste` exactly like
  `InsertCommand` — the text waits on the **paste-guard card** (Enter sends
  through `send_paste` → `normalize_paste`, Esc drops). Multi-line snippets are
  counted by lines. Snippets are user-authored, but they still pass the guard so
  a multi-line snippet cannot run unconfirmed at a shell prompt.

## Security

- Snippets never write to the PTY directly; they go through the same paste guard
  as palette commands and plugins (BAYAN-007 pattern).
- Snippet text is user config (trusted), distinct from output triggers which
  match untrusted output and may only notify.

## Verification

- `snippets_parse_and_round_trip` (config): the array parses, multi-line text
  survives, empty stays omitted on serialize.
- `the_snippet_menu_lists_saved_snippets_and_filters` (main): the menu lists
  rows, an empty name falls back to the text, the query filters.
- `no_snippets_opens_a_card_not_an_empty_menu` (main).
- Full suite + clippy + fmt green.
