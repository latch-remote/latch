# Viewing a machine

If a stream loses its connection unexpectedly, keep the window open. Latch
retries after 2, 5 and 10 seconds, then every 20–30 seconds with jitter for
up to five minutes. The overlay shows the countdown and attempt number.
Choose **Reconnect now** to skip the wait or **Stop** to return to machines
and cancel reconnection. After the recovery window, **Try again** starts a
new attempt.

A host returning online in the machine list makes a waiting retry immediate.
Returning to the app after a long pause also skips the wait. A reconnect
opens a new session and negotiates its display and network settings again.
A refusal, takeover by another viewer or an explicit end from the host stops
automatic retries; reconnect manually once the host is ready.
