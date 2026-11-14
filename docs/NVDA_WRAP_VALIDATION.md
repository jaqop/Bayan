# NVDA reading across terminal soft wraps

Base: main `1a8c788565b152d92b1b30e95d3f73f095abc756`.

## Cause and correction

The accessibility document previously appended a newline to every physical
grid row. NVDA therefore received `ونقا\nرن` instead of `ونقارن`, despite the
visual renderer keeping the word together. Wrapped-row trailing spaces were
also trimmed, potentially joining neighboring words when removing the break.

The snapshot now retains WRAPLINE and grid width. Normal Unicode output is
joined into logical paragraphs before building AccessKit TextRuns. Real
newlines and blank lines remain. Source columns are offset by original row
width, preserving caret and selection ownership after joining. Wide-character
wrap padding is excluded. Visual-order Claude output keeps its previous
per-row restoration path.

Selection endpoints from the grid are inclusive, while UIA ranges end after
the selected text. Converting that endpoint now includes the last character
and its combining marks. Text node IDs also have room for long paragraphs;
the old four-bit run-part field aliased another paragraph after 3200 chars.

Live announcements recognize a growing paragraph once its real newline
arrives. In-place typing is not announced as a fresh screen, and automatic
live tails remain bounded at whole-word boundaries. An overlong unbroken
token is omitted from the live summary; the full document remains available.

## Real Windows/NVDA comparison

NVDA 2026.3.1 portable, signed by NV Access Limited, was run with an isolated
configuration and eSpeak NG Arabic voice. The comparison used the same
1100x700 window, 14-point font and `mixed-arabic-english.txt` fixture, with
history markers disabled so all six paragraphs were visible.

The source contains 279 words and 1789 non-trailing Unicode characters,
including Arabic diacritics, English, paths, dates, digits and symbols.

Using the laptop keyboard layout:

1. Focus the Bayan pane.
2. NVDA+Ctrl+Home moves review to the document start.
3. NVDA+Shift+A reads all from review.
4. Inspect NVDA Speech Viewer and its actual speech IO log.

Before the fix, NVDA's speech stream contained physical-row breaks inside
words, including `ونقا\nرن` and `الن\nسخ`. Splitting the spoken source strings
on whitespace produced 289 fragments instead of the source's 279 words.

After the fix, all 279 word tokens matched the source in order, and every
nonempty paragraph matched. NVDA does not speak blank lines, so its captured
speech text has five fewer newline characters; the actual UIA document
matches the source exactly, including those blank lines.

Double-click selection of `النافذة.` was checked against Bayan's native
clipboard. NVDA+Shift+S reported `النافذة. selected`, including the final
letter and punctuation. This verifies the real UIA selection consumer, not
only the internal mapping test.

Local artifacts include sanitized fixture-only speech transcripts,
`nvda-speech-comparison.json`, executable identity records and Speech Viewer
evidence. Raw desktop-wide NVDA logs are not committed. This checks NVDA's
real reading pipeline; it is not a claim about every voice's pronunciation.

## Gates and scope

894 ordinary tests passed; seven explicit native/measurement probes remain
outside the default run. Formatting, all-target clippy with warnings denied,
build and Rust 1.89 all-target checking passed.

Regressions cover actual accessibility-tree text at 18, 70 and 102 columns,
hard newlines, spaces at wraps, emoji/wide-character padding, combining marks,
native caret/selection ownership, long-paragraph node identity, and bounded
live announcements. The reader still exposes the visible viewport, rather
than inventing text outside it. Existing rendering, shell security, OSC 52,
session and shell-menu behavior are retained.

## Mixed-language pronunciation

Arabic-script and ASCII English spans now carry `ar-SA` and `en-US`
accessibility language metadata. This lets NVDA select installed voices for
both languages instead of asking an Arabic voice to pronounce English.
Other scripts remain untagged; script tags are not general language detection.
Run boundaries and caret/selection positions use the same span calculation.
Text, real newlines and Unicode combining marks are preserved.

For listening, Windows OneCore can use Microsoft Zira as the English base
voice and installed Microsoft Naayf for Arabic language spans. A rate of 45
with rate boost disabled is the requested modest increase from 40. Voice
preferences belong to the screen reader and are not written to Bayan settings.
The earlier eSpeak comparison proves word preservation, not voice quality.

The updated full local suite passed 897 tests, with seven explicit probes
ignored. Added coverage checks language tags, all caret positions across many
script switches, combining marks and selections that cross language spans.
