# Known interoperability gaps

Each gate in `format.rs` retains the intended assertion and checks the exact
current output. An unexpected pass fails CI. Each result includes observed
and expected native values. Set a gate to `false` in its fixing PR and keep
the intended assertion.

| Case and gate | Direction | Observed | Expected | Fixing PR |
|---|---|---|---|---|
| `held-super-z`, `HELD_SUPER_GAP` | Windows/Linux → every host | Z down/up, mask 0, no Super event | VK_LWIN 0x5b down with mask 8, Z down/up with mask 8. Windows scan 0x5b extended, macOS key 55 with Command flag 0x100000, X11 keycode 133 | #109 |
| `right-shift-identity`, `RIGHT_SHIFT_GAP` | Every viewer → every host | VK_SHIFT 0x10 down with mask 1. Windows scan 0x2a, macOS key 56, X11 keycode 50 | VK_RSHIFT 0xa1 down with mask 1. Windows scan 0x36, macOS key 60, X11 keycode 62 | #109 |
| `clipboard-line-endings`, `CLIPBOARD_CRLF_GAP` | Windows → macOS/Linux | `first\r\néλ你😀\r\n` reaches the host write unchanged | `first\néλ你😀\n` reaches the host write | Issue #120, no fixing PR yet |

Windows/Linux held-Super supplies `Modifiers::NONE`, the egui events the
current capture boundary actually delivers for Win/Super+Z. egui has no
Super key or modifier there, and its `command` field aliases Ctrl. A native
key capture path is required. The right-Shift case supplies side-specific
user intent alongside the aggregated Shift state that reaches this boundary.
Mac Command retains its configured Ctrl mapping on Windows/Linux hosts.

## Updating after #109

Move the two modifier fixtures onto #109's native capture events, including
right-side Shift and physical held Win/Super. Flip `HELD_SUPER_GAP` and
`RIGHT_SHIFT_GAP` to `false`. Remove their rows above. Preserve their intended
wire/native values in the Expected column. Replace the old observed VK_SHIFT
0x10 with VK_RSHIFT 0xa1, and replace unmodified Z with the held VK_LWIN/mask-8
sequence. The ordinary left-modifier fixtures must use the new sided aliases
VK_LSHIFT 0xa0, VK_LCONTROL 0xa2 and VK_LMENU 0xa4 and extend the independent
native reference table accordingly. Keep the clipboard gate enabled until
its separate host newline-normalization fix lands. The clipboard gap is
tracked in issue #120 and has no fixing PR as of this revision.

The X11 scroll-unit cases pass. A wire notch of 120 becomes one press/release
of button 4 for up and button 6 for left. Two 20-point deltas become two
60-unit wire events and one accumulated X11 notch. X1/X2 map independently
to X11 buttons 8/9, Windows XBUTTON1/2 and macOS buttons 3/4.


## Reproducing the tracked follow-ups

### Clipboard CRLF

Use run 43697534260's `pairs-recording-windows-x86_64` artifact as the viewer
input. Its `macos.input.json` and `linux.input.json` contain the paste text
`first\r\néλ你😀\r\n`, obtained through Event::Paste, StreamView::paste,
clipboard push/fitting and JSON encoding. The host write receives that exact
CRLF string. The intended macOS/Linux host write is `first\néλ你😀\n`.
The reproducing jobs are `Replay on macos-aarch64` (132763113752),
`Replay on linux-x86_64` (132763113756) and
`Replay on linux-aarch64` (132763113775). Read their
`clipboard.jsonl` row with producer `windows-x86_64` and case
`clipboard-line-endings`. The exact observed value and its mismatch with the
intended value are asserted as a known gap. Tracked in issue #120.
