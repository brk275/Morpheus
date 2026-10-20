# The call bar

Status: **in development**. Source only: nothing of it has been built or run
yet. Hosted UI checks and acceptance in real calls are pending.

While you are in a call, a compact bar stays under the page header on every
page except **Voice & video**: in a conversation, in Settings and in
Discover rooms. The conversation, its composer, the account switcher and the
panes keep their places and sizes.

The view is `crates/morpheus-app/ui/call-bar.slint` and its controller is
`crates/morpheus-app/src/call_bar.rs`.

## What it shows

- **The call's room and state**: connecting, connected, reconnecting (with
  what it is recovering from), leaving, or an incomplete cleanup. The state
  is the call's actual status.
- **Time in this call**, while connected. It counts from when this device saw
  the call joined, and keeps counting while the same call reconnects. It is
  not the time anyone else has been in the call. It is not saved, logged or
  sent, and it stops with the call.
- **How many people the call lists**, when the call reports them and the bar
  is wide enough.
- **What this device sends**: microphone on or muted, camera on (or starting,
  stopping), screen shared (or choosing a screen, starting, stopping). These
  come from what is actually captured, never from a setting. They never
  disappear for lack of space; a narrow window shows them as icons with
  their labels for assistive technology.

## Controls

The bar offers the call page's controls:

- Mute or unmute the microphone.
- Deafen or undeafen the speaker (on wider windows).
- Stop the camera, or stop sharing the screen. These appear only while that
  capture runs. They never start one or open a device picker.
- **Return** shows the call page and moves keyboard focus to its stage: the
  featured tile, else the first tile, else the stage's own Show or Hide
  participants control, else (a narrow stage with nobody's video) the stage
  itself, which then shows a focus ring and is announced as the call stage.
  It never joins, rejoins or turns anything on, and moving focus pins, pages,
  shows or hides nothing.
- **Leave** leaves the call.

Each control acts only on the exact call the bar is showing: that account,
room and call, and for the microphone, speaker and capture controls, the
connection of it that was on screen. Another call never counts as the same
one, even in the same room and account. The call controller checks this
again at the moment it acts, so a call that was replaced after the click is
left alone and the bar says nothing was done. Mute and Deafen also wait until
the microphone and speaker state on screen belongs to that same connection,
and the Stop buttons until the camera and screen state on screen does: right
after a new connection appears, until its state is shown, they do nothing.
A Stop never waits in line: if the call's capture is still handling an
earlier request, nothing is sent and the bar says so. A Stop that was sent
stops that capture of the same call, even if the call reconnects meanwhile;
it never reaches another call.

A click also carries the account and security state it was made in. If the
account changes, a security step (account access, recovery, restart)
starts, or a protected screen or dialog opens before the call controller
acts, the click is dropped, even if that step has ended or that dialog has
closed again by then. Every way of opening one drops waiting clicks at that
very moment, before it appears. A Leave or Stop clicked while the account or
a security step is closed to it says so, and nothing is done.

While a call is still being set up, while it is leaving, and after an
incomplete cleanup, the exact call is not known yet or any more. Leave and
the media controls are then disabled, and Leave's label says to use the call
page; Return still leads there.

All controls are reachable with Tab and Shift+Tab, in order, and have spoken
labels. The bar uses the theme's colors, fonts and text size, including the
theme's hover, pressed and disabled control colors and heavier outlines in
high contrast, and has no motion. In a narrow window or at large text, its
controls move to a second row and the capture indicators become icons with
spoken labels; down to the smallest window (640 by 620) at 200% text,
everything stays inside the window's top half.

## Protected screens

The bar is never shown when no session is live or while anything else
takes the window over: sign-in, Manage accounts, the local app password, a
pending account switch, the account menu and profile, the header's More room
actions menu, a security step, a
recovery key, device verification, People, member details, a Matrix link
card, the Space explorer and Space rooms, room addresses, room
notifications, Seen by, the Quick Switcher, room actions, room
administration (including roles and permissions), a member confirmation,
the capture source picker, the leave dialog, an image preview, the poll
dialog and the thread browser, and in Settings the ignored-users review and
shortcut recording. It stays available everywhere else, including while you move between
rooms and pages. If a call is active or the camera or screen share runs
then, only a small badge says so: "Call active · Mic on · Camera on", for
example. It names no room, account or person and has no controls. It never
grows wider than the window; at large text its words wrap onto another line
rather than being cut off.

## Limits

- The bar picks up a new connection of the same call (after a rejoin) with
  the call's next status. Until then its microphone, speaker and capture
  controls do nothing and the bar does not apply them to the new connection;
  Leave still works.
- The incomplete-cleanup review is only on the call page.
- Video is not shown in the bar.
