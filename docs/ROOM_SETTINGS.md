# Room settings

Room settings are part of the pre-alpha administration work. The new avatar and
access-rule controls require hosted native and live-server verification before
they can count toward beta readiness.

## Name and topic

Names are trimmed, must not be empty and are limited to 1024 UTF-8 bytes. Topics
preserve whitespace, may be cleared and are limited to 16 KiB. Each field saves
separately. Saving one field retains the loaded values and conflict checks for
the other fields; use Refresh to load all current server values.

## Room avatar

Choose a static PNG or baseline JPEG, review the staged image, then save it.
Selecting a file does not upload it. Discard removes the staged selection;
removing the room's current avatar requires confirmation.

Input is limited to 8 MiB, 4096 pixels per side and 4 megapixels. The app decodes
the image off the UI thread, reduces its longer edge to at most 512 pixels
without upscaling, and creates a new PNG without embedded metadata. Aspect ratio
and transparency are preserved within pixel rounding. Animation, progressive
JPEG, WebP, HEIC and SVG are unsupported. EXIF orientation is not applied; check
the staged preview before saving.

A room avatar is shared room metadata, including in rooms whose messages are
encrypted. Current avatars are downloaded only on an explicit preview action.
The encrypted-room image-preview preference also applies here. Preview and staged
image data stay in memory and are cleared when the form closes or its account,
room or preview permission changes.

## Who can join

The available choices depend on the room version and your current permissions.

| Access rule | Behavior |
| --- | --- |
| Invite | Joining requires an invitation. |
| Public | Anyone who can reach the room may join; saving requires confirmation. |
| Knock | People can request an invitation. |
| Restricted | Members of the listed rooms may join, subject to the homeserver's authorization rules. |
| Knock restricted | Members of the listed rooms may join; others can request an invitation. |

Restricted rules accept up to 32 distinct Matrix room IDs. A Space is also a
Matrix room and can be used in that list. Unknown rules, unsupported allow-list
entries and unsupported room versions remain read-only. Changing the join rule
does not publish the room in a public directory.

## History, guests and directory listing

Access and visibility, opened from Room settings, reads three settings fresh
from your homeserver and shows each with the choices you can make:

- **Who can read the history**: members from when they joined, members from
  when they were invited, members including messages from before they joined,
  or anyone, even without joining. A change applies to messages sent from then
  on. People keep what they could already read, and in encrypted rooms it shares
  no message keys, so new members may still be unable to read earlier messages.
- **Guest access**: whether guest accounts may join. Guests still need what the
  join rule requires (an invitation in an invite-only room) and a homeserver
  that offers guest accounts. Allowing guests creates no accounts and gives no
  one keys to encrypted messages.
- **Room directory**: whether your homeserver lists the room in its room
  directory, where anyone who browses it can find the room's name, topic,
  avatar, address and member count. Listing does not change who may join. Only
  your own homeserver's directory is covered. If it offers no directory, or
  gives no usable answer, the panel says so and the listing cannot be changed.

A setting the room has never set shows the Matrix default, and so does guest
access redacted to no value. A history-visibility event without a value, a
value Morpheus does not recognize, or content it would not rewrite is
read-only; so is a setting your permissions in the room do not allow. Who may change the directory listing
is up to the homeserver: Morpheus first asks for the room's address permission,
as Synapse does, and the homeserver's answer is final. These reads, the
permission checks behind them included, never renew an expired sign-in to read
again.

Choosing sends nothing. Review changes lists each change and what it means.
Making the history readable by anyone, or listing the room, also needs an
explicit confirmation. Save then sends the changes one at a time: those that
close the room up first, and a listing last. Each is sent only after a fresh
read finds that setting exactly as reviewed, and the first that does not
succeed stops the save; the rest are not sent. Results are reported as
described in [How a save is sent](#how-a-save-is-sent), and after any failure
the panel needs a refresh before another change.

A change counts as saved only when the settings read back show it, together
with every change saved before it. A change your homeserver accepted that the
read-back does not show is reported as accepted but not confirmed, and the rest
are not sent. Closing the panel stops a read; closing the form stops a change
that has not been sent yet. A change already sent is still waited for, and its
result is kept for as long as Morpheus runs, whether the form is closed,
reopened or refreshed, or another account is shown: a setting whose earlier
change is still waiting stays read-only with a note until that change is
answered. One that got no answer stays read-only until Morpheus restarts, even
if a later read shows the value you chose, because the unanswered request may
still apply after that read. Removing the account from Morpheus completely, or
logging out of it, forgets these results; a removal that does not complete
keeps them. Nothing is sent again automatically, and a read cannot make a change
and its read-back atomic.

## Permissions and concurrent edits

The form reads current room state from the homeserver. Your permissions may
change while it is open; the server remains authoritative. Each save checks the
account and form, refreshes permission information, and rejects an observed
change to that field since it was loaded. This detects stale edits but cannot
make a read and write atomic across clients.

## How a save is sent

Each save is one request to the homeserver; saving an avatar adds one image
upload before it, sent as it is: the homeserver refuses an image larger than it
accepts. Morpheus does not retry either one, does not renew an expired
sign-in to send it again, waits a bounded time and accepts only a small answer.
Right before each request is sent, after every other step, the form checks that
it is still open, that the same account is still selected, that you are still
in the room, that the fresh permission read allowed the change and that no
account or security step is in progress. If any check fails, nothing is sent.

Once a request is sent, the result shown is what is known:

- **Saved**: the homeserver applied it. If the settings could not be read back
  afterwards, the form says so.
- **Refused or limited**: the homeserver answered that it did not apply it.
- **Unknown**: no answer came back, the answer could not be read, or the form
  closed or the account changed while the request was on its way. The setting
  may or may not have changed, and the request is not sent again. For an
  avatar, the form says when the image was uploaded but whether the room
  avatar changed is unknown.
- **Image uploaded, avatar not changed**: the image reached your homeserver,
  but the room avatar was not changed (for example, a security step started
  before the change was sent). The image is not uploaded again unless you
  save again.
- **Image may have been uploaded, avatar not changed**: the image upload was
  sent but no usable answer came back, or the form closed or the account
  changed while it was on its way. The image may be on your homeserver; the
  room avatar was not changed, because the change is sent only after a
  confirmed upload. The image is not uploaded again unless you save again.

A save changes only the field it is for. Anything else another app stored in
that setting, including fields Morpheus does not know and the exact spelling
of numbers, is kept as it was; a topic's plain and formatted forms are both
replaced. Settings content that is not a plain object, or names a field twice,
is not rewritten. A setting counts as changed on the server if its stored text
differs in any way from what the form loaded, even only in how a number is
written. Every read of room settings is limited in size while it arrives; a
larger answer is refused as unsupported.

After any failed or uncertain save, refresh before trying again: a refresh shows
what the room has. Closing the form cancels pending work; it cannot undo a
request the server has received. An interrupted avatar upload can leave an
unused media object without changing the room's avatar.

See [the feature matrix](FEATURE_MATRIX.md) and [release gates](RELEASE_CHECKLIST.md)
for the broader administration and platform acceptance requirements.
