# Incoming calls

The running native app shows admitted incoming calls for the active account.
**Answer** joins the exact offered call. **Decline** tells the caller: it sends
one decline for that call's notification and removes the offer.
**Dismiss** removes the offer on this device only; it sends nothing, so the
caller and your other devices are not told. Joining a voice space does not ring
its members.

## Decline

Decline uses the MatrixRTC decline event (MSC4310,
`org.matrix.msc4310.rtc.decline`, a reference to the call's
`m.rtc.notification`) as the pinned Matrix SDK builds and sends it.

- It is sent once, as the account that received the call, in that call's
  encrypted room. There is no queue, retry or later replay: if the homeserver
  rejects the session's access token, the decline is reported as not accepted
  and is not resent after the session is refreshed. It never joins the call,
  opens a microphone, camera or screen, or changes permissions.
- Before the notification is fetched and again right before the send, the
  original offer must still hold: the same account selection, the room still
  joined and encrypted, the same retained notification and caller, a still
  trusted caller device, no security step since the click, and an unexpired,
  unreplaced offer. Switching or locking the account, removing it, a policy
  change or quitting cancels the attempt; it is never retried for a newer
  selection. While the send itself runs (including any encryption key sharing
  the SDK does first), these conditions are checked whenever Morpheus resumes
  the send. Once one is lost the send is not resumed and the result is reported
  as unknown. A request already handed to the network cannot be withdrawn.
- The banner shows “Declining the call…” while the send runs, then one fixed
  result: “Call declined.” only once the homeserver has returned the event's
  ID; otherwise that nothing was sent, that the server did not accept it (only
  for a client error with a known rejection code, such as an expired session,
  missing permission, rate limiting or a malformed request), or that it is not
  known whether the decline arrived (any server error, other answer, network
  failure or timeout; the caller may still see it ringing). Nothing is retried.
  Results never include server messages or identifiers.

## Declined on another device

When you decline a call on another of your verified devices, it stops ringing
here and its offer goes away. Its desktop alert can no longer open anything; on
Linux it is also withdrawn where the system allows, but on macOS and Windows an
alert already shown may stay on screen until you close it. Nothing is sent,
joined or opened on this device.

- Only an encrypted decline of exactly that call, received through this
  account's sync, counts. It must be verified at decryption as sent by you from
  another device (not this one, and not decrypted with forwarded keys), and
  that device must still be cross-signing verified, with the same keys, when
  the offer is removed. Plaintext, undecryptable, redacted, unverified or
  another person's declines, and events fetched from history, are ignored. A
  decline that arrives in a catch-up batch of the sync still counts, but only
  for a call that is ringing now.
- A decline can arrive in the same sync as its call, before the offer is
  shown. A buffer of up to 16 waiting declines is kept in memory for at most
  two minutes, for the active account only (declines whose trust is being
  checked are held separately while that check runs); switching accounts or
  quitting discards them. Each device's decline of a call is checked once;
  repeats of it are ignored. While that check runs (at most a few seconds), a
  call not yet shown waits; a call already ringing keeps ringing until the
  decline is confirmed. Answering or dismissing other calls is never held up,
  and a decline for one call never interrupts or loses another incoming call.
  A decline that is not kept, or whose device is no longer trusted, leaves the
  call ringing until it is answered, declined, dismissed or expires.
- A call you are already answering or declining on this device is not
  affected.

## Sound

The native-call build now includes an original, quiet two-note ringtone. This
implementation has been source-reviewed; native compilation, regression tests
and actual audible output have not yet been verified.

- A room's message notification level (all messages, mentions and keywords,
  or muted) does not change whether its calls ring; see
  [notifications](NOTIFICATIONS.md#one-room-the-header-bell).
- Sound requires both notifications and sounds to be enabled for the account.
  It uses the configured speaker, or the default speaker when none is selected.
  An unavailable selected device does not fall back to another speaker.
- A call rings at most once, for no more than 30 seconds or the offer's remaining
  lifetime, whichever is shorter. Refreshing the list or re-enabling sound does
  not replay an old offer. Only one offer can sound at a time; other offers stay
  visible without queuing later sounds.
- Answer, Decline, Dismiss, a decline on your other verified device,
  withdrawal, expiry, account revocation and app shutdown stop the sound. Starting an outgoing call also suppresses it immediately, including
  while the request waits for another account operation. Recovery/security and
  update/call reservations suppress sound as well.
- Preview mode is silent. Ringing opens only speaker output; it does not open a
  microphone, join a call or transmit media.
- A sound failure leaves the offer answerable and displays the fixed message
  “Incoming call sound is unavailable. You can still answer.”

These sounds use native audio output, separate from OS notification banners.
They do not add integration with system Focus/Do Not Disturb policies. Turn off
Morpheus's notification sounds to silence them. Background-account ringing
remains unfinished work. A fully quit app has no ringtone producer.

## Desktop alert

Each admitted offer for the account on screen also shows one silent
operating-system banner, through the same native presenter as message banners.
Its title is “Incoming call · ” and the account's Matrix ID. Its text names the
caller's user name only where Settings › Notifications shows room and sender,
and never while sign-in, the app password, Manage accounts or a recovery key
is on screen; otherwise it reads “Open Morpheus to see the call”. It never
names a room or anything the call event says.

- It follows the same explicit call alert settings as the ring: the global
  notification switch and This account's `notifications_enabled` override. A
  room's message level and its local quiet, preview or highlight choices do not
  change it. The banner itself is always silent: the ringtone above is the only
  sound, and the sounds switch adds none to the banner.
- Refreshing the list never shows an offer twice. A security step running when
  the offer first appears, an account switch, lock or removal, or the offer
  ending first withholds its banner; nothing is queued for later.
- Clicking it only brings a minimized Morpheus window back. It never answers,
  joins, declines, switches or unlocks an account, opens a device or captures
  media; Answer and Dismiss stay on the incoming card. The click does nothing
  unless, at that moment, the same account selection is on screen, no security
  step began since the offer appeared, no private screen is up, alerts are
  still on and the offer is still current. Switching away and back never
  revives an old banner. On macOS the system itself brings Morpheus forward
  when any of its banners is clicked, even an old one; Morpheus then changes
  nothing.
- When the offer is answered, dismissed, declined, expires or is withdrawn,
  the account changes or Morpheus quits, its banner is forgotten and its click
  opens nothing. Only Linux can withdraw a banner, and only one that Morpheus
  is listening to (the desktop service returned it with a listener in place);
  any other banner, and every macOS and Windows banner, can stay on screen.
- The account is read first and the window last, both right before the banner
  is handed over and again after the operating system call returns. If alerts
  were turned off, or the caller may no longer be named (room and sender
  hidden, or a private screen opened), the banner is forgotten and withdrawn
  the same way; a banner already shown is checked about once a second.
  Morpheus cannot take back text the operating system already received, and
  never shows the banner again with less text.
- The operating system call runs on its own thread, one at a time, and is given
  up after ten seconds; quitting never waits for it. A banner it shows after
  that, or after Morpheus began to quit, opens nothing, and is withdrawn where
  that is possible as above. Right before the send itself (on macOS later,
  when Morpheus starts waiting for the click), that thread reads the window
  again, waiting at most two seconds, and checks it with the account,
  security step, offer and settings; no answer in time, a closed window or
  quitting sends nothing. A change in the instant between that check and the
  send can still be sent.
- If a banner is not confirmed (the operating system failed it, no click
  listener was free, the macOS send failed, or the call did not return within
  ten seconds), the incoming card's notice line reads “Could not confirm a
  desktop alert for this call. It is still shown here.” while the offer lasts.
  A slow call may still show its banner later. No backend or server text is
  shown or logged.
- Call banners share the presenter's eight click listeners with message
  banners; on Linux the oldest withdrawable banner frees one, otherwise a
  Linux or Windows banner can be shown without click handling. On macOS a
  call banner is sent only by a free listener, which reports its result; with
  none free it is not sent. At most eight call banners are remembered and four
  wait for the operating system; none opens anything after two minutes. Only
  identifiers are kept, nothing is persisted, and a quit app shows no banner.

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
outgoing-call handoff, withdrawal, expiry, account changes and quit. Decline has
source review and five small regression additions (claim exclusivity and
cancellation, bounded command replies, fixed result text, rejection
classification, and fenced polling of the send) that have not run;
its real send, the caller's handling of it and each result path against a
homeserver are unverified. Stopping on another device's decline has source
review and eight regression additions (receipt qualification, bounded
retained declines, retention cleanup across stop and account changes, held
trust checks with replayed declines, exact settlement before the decline's own
deadline, another call's admission surviving a decline's arrival, checks
finishing while another call is admitted, and new declines checked and applied
while another call is admitted) that
have not run; a real decline from a second verified
device, same-sync ordering and trust changes are unverified. The subtle
status announcement also needs keyboard/screen-reader acceptance. Source review
does not establish these runtime results.

The desktop alert's record, text, settings, privacy-order, handover-sample,
late-failure, delivery-slot and late-completion cases are new unit tests that
have not run. They use in-test stand-ins for the window and handles, not the
operating system. Whether its banners appear, stay silent, open the
window and are withdrawn on each platform, including under Focus/Do Not
Disturb and a denied notification permission, remains to be accepted.
