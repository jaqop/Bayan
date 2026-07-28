# BiDi fixture corpus

`bidi_cases.json` is a language-agnostic test corpus for bidirectional Arabic
text in a terminal grid. It was generated from `src/bidi.rs` by a temporary
emitter that asserted every pair round-trips through the real implementation
before printing it — so these are proven pairs, not hand-transcribed ones. The
codepoint fields were then re-derived programmatically from the strings.

## Reading it

Each case gives two representations of the **same text**:

| field | meaning |
|---|---|
| `logical` | the order bytes arrive in and sit in memory |
| `visual` | the order a UAX#9-applying renderer puts on screen |

A terminal that applies BiDi maps **logical → visual**. A terminal undoing a
client that already applied BiDi (Bayan's Claude mode) maps **visual → logical**.
The pairs serve both; only the arrow changes.

Codepoints are listed explicitly because editors and terminals reorder RTL text
on display — you cannot eyeball an Arabic fixture file and know what it says.

## What each case pins

- **`arabic_word` / `arabic_sentence`** — baseline whole-line reversal; word
  order reverses along with letter order.
- **`combining_tanwin`** — the cluster rule. U+064B must stay immediately after
  its base U+0627 in *both* representations. A codepoint-level reverse emits the
  mark before its base and renders as garbage that is hard to trace back to the
  reversal step. Reorder grapheme clusters, not codepoints.
- **`arabic_indic_digits`** — the digit rule. U+0660–0669 and U+06F0–06F9 are
  weak LTR and keep their internal order. An "is this Arabic?" helper that
  range-checks the whole U+0600–06FF block sweeps digits in and reverses every
  number on the line.
- **`ltr_island_path`** — the island rule. `config.txt` survives intact inside an
  RTL-base line because the dot is absorbed into the Latin run *only when a Latin
  character follows it*. Absorb punctuation unconditionally and a trailing period
  flips: `01.` becomes `.01`.
- **`ltr_base_arabic_tail`** — the base-direction rule. More Latin than Arabic
  means an LTR base, so only the Arabic run reverses, in place. Contrast
  `arabic_sentence`, where the whole line reverses.
- **`no_arabic_untouched`** — a line with no Arabic must come back byte-identical.
  Guards against a transform that runs unconditionally.

`multiline_case` covers selection and copy across lines of differing base
direction: each line classifies independently, so a whole-block transform is
wrong. `program_gating` is not BiDi, but records that a compatibility mode for
clients which pre-apply BiDi must key on the *invoked program* — basename,
extension stripped, runner prefixes unwrapped — or `vim claude.py` silently
enables it.

## Provenance

Bayan is Rust and solves the inverse transform; anyone applying BiDi forward
(Ghostty's `BiDi.zig` + FriBidi, for instance) cannot reuse the code, but the
pairs port directly. Shared into
[ghostty-org/ghostty#9774](https://github.com/ghostty-org/ghostty/discussions/9774).
