# Incoming calls

The running native app shows admitted incoming calls for the active account.
**Answer** joins the exact offered call; **Dismiss** removes the local offer.
Dismiss does not send a protocol Decline message. Joining a voice space does
not ring its members.

## Sound

The native-call build now includes an original, quiet two-note ringtone. This
implementation has been source-reviewed; native compilation, regression tests
and actual audible output have not yet been verified.

- Sound requires both notifications and sounds to be enabled for the account.
  It uses the configured speaker, or the default speaker when none is selected.
  An unavailable selected device does not fall back to another speaker.
- A call rings at most once, for no more than 30 seconds or the offer's remaining
  lifetime, whichever is shorter. Refreshing the list or re-enabling sound does
  not replay an old offer. Only one offer can sound at a time; other offers stay
  visible without queuing later sounds.
- Answer, Dismiss, withdrawal, expiry, account revocation and app shutdown stop
  the sound. Starting an outgoing call also suppresses it immediately, including
  while the request waits for another account operation. Recovery/security and
  update/call reservations suppress sound as well.
- Preview mode is silent. Ringing opens only speaker output; it does not open a
  microphone, join a call or transmit media.
- A sound failure leaves the offer answerable and displays the fixed message
  “Incoming call sound is unavailable. You can still answer.”

These sounds use native audio output, separate from OS notification banners.
They do not add integration with system Focus/Do Not Disturb policies. Turn off
Morpheus's notification sounds to silence them. Incoming OS notifications and
background-account ringing remain separate, unfinished work. A fully quit app
has no ringtone producer.

## Ownership and cancellation

Only the authenticated incoming-call controller can create the revocable sound
lease. Refreshed snapshots share a single claim, the original deadline and the
exact account selection. Account, policy, security and call changes invalidate
the playing lease permanently; a later transition back cannot revive it.

One native owner thread holds the sole output permit through device and stream
destruction. Authorization is checked around each device lookup/configuration
step and for every callback frame. Nonmatching devices are not queried for
output capabilities. The callback allocates and locks nothing, and writes silence
after cancellation. Already-submitted hardware samples and an OS driver call
already in progress cannot be recalled. A stuck driver retains the permit
instead of allowing more opener threads to accumulate.

The synthesized sound uses no downloaded audio, bundled media asset or new
dependency. It supports 48 kHz mono/stereo output in F32, I16 or U16 formats.
Unsupported devices produce the fixed unavailable message above.

## Verification still required

Fifteen new regression sources cover synthesis, PCM silence, cancellation,
expiry, single-use leases, account/policy/security changes, identity-first device
selection and pending outgoing-call suppression. An existing call-reservation
test also checks sound exclusion. These tests have not run yet.

Release acceptance still requires native tests on all four targets, actual
speaker routing and volume, permission/unplug/backend failures, Answer and
outgoing-call handoff, withdrawal, expiry, account changes and quit. The subtle
status announcement also needs keyboard/screen-reader acceptance. Source review
does not establish these runtime results.
