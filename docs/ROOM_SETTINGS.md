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

## Permissions and concurrent edits

The form reads current room state from the homeserver. Your permissions may
change while it is open; the server remains authoritative. Each save checks the
account and form, refreshes permission information, and rejects an observed
change to that field since it was loaded. This detects stale edits but cannot
make a read and write atomic across clients.

After an uncertain or failed save, refresh before trying again. A request already
sent may have been applied even if the app did not receive its result. Closing
the form cancels pending work; it cannot undo a request the server has received.
An interrupted avatar upload can leave an unused media object without changing
the room's avatar.

See [the feature matrix](FEATURE_MATRIX.md) and [release gates](RELEASE_CHECKLIST.md)
for the broader administration and platform acceptance requirements.
