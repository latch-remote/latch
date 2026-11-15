# Known interoperability gaps

Each gate in `format.rs` retains the intended assertion and checks the exact
current output. An unexpected pass fails CI. Each result includes observed
and expected native values. Set a gate to `false` in its fixing PR and keep
the intended assertion.

| Case and gate | Direction | Observed | Expected | Fixing PR |
|---|---|---|---|---|
| `held-super-z`, `HELD_SUPER_GAP` | Windows/Linux → every host | Z down/up, mask 0, no Super event | VK_LWIN 0x5b down with mask 8, Z down/up with mask 8. Windows scan 0x5b extended, macOS key 55 with Command flag 0x100000, X11 keycode 133 | #109 |
| `right-shift-identity`, `RIGHT_SHIFT_GAP` | Every viewer → every host | VK_SHIFT 0x10 down with mask 1. Windows scan 0x2a, macOS key 56, X11 keycode 50 | VK_RSHIFT 0xa1 down with mask 1. Windows scan 0x36, macOS key 60, X11 keycode 62 | #109 |
| `clipboard-line-endings`, `CLIPBOARD_CRLF_GAP` | Windows → macOS/Linux | `first\r\néλ你😀\r\n` reaches the host write unchanged | `first\néλ你😀\n` reaches the host write | Follow-up PR unassigned |
| `mac-vt-mf-native`, `MAC_VT_MF_GAP` | Mac VideoToolbox native/capture → Windows Media Foundation decoder, every schedule | Only calls 6, 12 and 18 publish pictures. Their content is stale frames 5/11/17, or 3/11/17 after intentional loss. Call 12 still has the old 160×98 size | All 24 pictures for clean/late-audio, or 22 after intentional loss of frames 4/5. The exact stale-content sequence and size are checked at ≥40 dB per plane, while the full intended sequence/size remains in results. Audio/FEC/causal-keyframe checks still run | Follow-up PR unassigned |

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
its separate host newline-normalization fix lands. Keep `MAC_VT_MF_GAP`
enabled until the decoder interoperability follow-up fixes all six cells. No clipboard fixing PR
has been opened as of this revision.

The X11 scroll-unit cases pass. A wire notch of 120 becomes one press/release
of button 4 for up and button 6 for left. Two 20-point deltas become two
60-unit wire events and one accumulated X11 notch. X1/X2 map independently
to X11 buttons 8/9, Windows XBUTTON1/2 and macOS buttons 3/4.


## Reproducing the unassigned follow-ups

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
intended value are asserted as a known gap. The fixing PR is unassigned.

### Mac VideoToolbox to Windows Media Foundation

Run 43697534260, job `Replay on windows-x86_64` (132763113745), fails the
`native` decoder cells for the macOS producer's `native.session.json` and
`capture.session.json`, in `clean`, `loss-reorder` and `late-audio` schedules.
The publication IDs are exactly `[6, 12, 18]`. Their contents lag to source
frames `[5, 11, 17]`, or `[3, 11, 17]` for loss/reorder. Intended IDs are 0 through
23, omitting only 4 and 5 for loss/reorder. The original failure reads
`assertion left == right failed: decoded identities [6, 12, 18]`, with
`left: 3` and `right: 24` (or `22` for loss/reorder).

Run 43699413179, job `Replay on windows-x86_64` (132769891339), exposes
stale output after asserting that publication sequence. Capture fails at
call 12 with `assertion left == right failed: frame identity/order/cropping`,
`left: (12, 160, 98)` and `right: (12, 128, 74)`. Native clean/late-audio
fails at call 6 with `frame 6 visible-plane PSNR 37.93794473823191`.
Native loss/reorder reports `frame 6 visible-plane PSNR 28.397404091367946`,
against the intended ≥40 dB. The original full-sequence, size and quality
expectations remain recorded alongside the precise stale-content snapshot.

Both tapes negotiate `H264Baseline` and carry Annex B H.264 Constrained
Baseline, profile_idc 66 (0x42), constraint byte 0xc0, level_idc 21 (2.1),
8-bit 4:2:0. There are no B frames. SPS has one reference picture,
log2_max_frame_num 5, pic_order_cnt_type 0 and log2_max_pic_order_cnt_lsb 6.
Coded sizes are 192×112 then 192×96. Right/bottom crop offsets in luma samples
are 32/14 then 64/22, producing the negotiated visible sizes. VUI colour
primaries/transfer/matrix are 1/1/1, limited range. Timing and HRD are absent,
and bitstream_restriction_flag is 0, so max_num_reorder_frames is unspecified.
SPS/PPS/SEI/IDR NAL types 7/8/6/5 repeat
at frames 0, 6, 12 and 18. Other frames carry type-1 slices. The visible crop
changes from 160×98 to 128×74 at frame 12. The SPS NAL hex, including its
0x27 header, is `2742c015ab4187f08c44d404040408` before resize and
`2742c015ab4186f043193501010102` after resize. Both software consumers and
the Mac VideoToolbox consumer decode the full intended sequence. The fixing
PR is unassigned.

On a Windows hosted runner, download the Windows binaries and Mac recording
from the same pairs run, then select one cell directly. Set
`LATCH_PAIR_ARTIFACTS` to the recording root, `LATCH_PAIR_CELL` to the full
path of `native.session.json` or `capture.session.json`,
`LATCH_PAIR_DECODER=native`, `LATCH_PAIR_SCHEDULE=clean` (or either impaired
schedule) and `LATCH_PAIR_RESULTS` to an empty output directory. Run
`target/pairs/bin/stream.exe pair_replay --ignored --nocapture --test-threads=1`.
The current known-gap row includes observed/expected frame lists, the strict
backend name and per-frame Media Foundation decode errors, while retaining
picture quality and all other session assertions.
