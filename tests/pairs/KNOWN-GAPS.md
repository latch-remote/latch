# Known input interoperability gaps

These cases retain their intended assertions and assert the exact current
output separately. A passing known-gap case fails CI until its entry and
expectation are updated in the fixing PR. Each consumer's JSON results
contain the corresponding native observed and expected values.

| Case | Viewer → host | Observed | Expected |
|---|---|---|---|
| `held-super-z` | Windows/Linux → Windows/macOS/Linux | Z down/up, modifier mask 0, no held Super event | VK_LWIN down with mask 8, Z down/up with mask 8. Native Super is Windows scan 0x5b with extended flag, macOS key 55 with Command flag 0x100000, X11 keycode 133 |
| `right-shift-identity` | Every viewer → every host | VK_SHIFT 0x10 down with mask 1. Windows scan 0x2a, macOS key 56, X11 keycode 50 | VK_RSHIFT 0xa1 down with mask 1. Windows scan 0x36, macOS key 60, X11 keycode 62 |
| `clipboard-line-endings` | Windows → macOS/Linux | `first\r\néλ你😀\r\n` reaches the host write unchanged | `first\néλ你😀\n` reaches the host write |

The right-Shift case supplies the user's side-specific intent alongside
egui's aggregated Shift state. The current viewer boundary has already
lost that identity. Mac Command keeps its configured mapping to Ctrl on
Windows/Linux hosts and to Command on a Mac host.

The audited X11 scroll-unit suspicion passes the recorded line and
accumulated point cases. A wire notch of 120 becomes one press/release of
button 4 for up, and button 6 for left. Two 20-point vertical deltas become
two 60-unit wire events and one accumulated X11 notch. The protocol
continues to accumulate sub-notch movement into discrete X11 wheel buttons.
