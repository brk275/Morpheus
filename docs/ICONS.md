# Interface icons

**Status: source only. Not built or rendered yet.** Root does the rendered, screenshot and platform checks.

Morpheus draws its chrome icons as original vector paths written for the app. They live in `crates/morpheus-app/ui/icons.slint`. No icon pack, generated image, remote URL or runtime SVG loading is used. Emoji, avatars, initials, custom room emoji and reactions stay content and are never replaced by icons.

## Using an icon

```slint
import { MorpheusIcon } from "icons.slint";

MorpheusIcon { width: 20px; height: 20px; icon: "search"; ink: root.icon-ink; }
```

- `icon` takes a stable ID from the list below. An unknown ID draws nothing.
- `ink` is the host's current state color, such as the icon, active-icon, selection-ink or disabled tokens. The icon never picks its own color.
- `filled: true` with `fill-ink` fills the closed shapes, for example for a selected state.
- The icon is decorative (`accessible-role: none`). The hosting control keeps its label, focus ring, hit target and guarded action.
- `IconAction { icon: "..." }` accepts an ID directly. A legacy one-glyph caption still maps through `MorpheusIcons.from-legacy`. Any other string, such as `Md` or `10k`, keeps the bounded text fallback.

## Geometry and states

- Every icon is drawn in a 24-unit square with a 2-unit margin, so all icons share the same optical bounds at 16, 20, 24 and 32 px.
- Stroke width is `clamp(size * 0.0625 + 0.5 px, 1.25 px, 2.75 px)`: 1.5, 1.75, 2.0 and 2.5 px at those sizes. High contrast adds 0.5 px.
- Strokes use round caps and joins.
- Icons never animate, so reduced motion needs no special case.
- `IconAction` sizes icons from the existing `geometry.icon_size` token.
- No new preference was added. A stroke-weight preference would need to be coordinated with appearance and go through root's schema review first.

## Stable IDs

Add IDs only. Never rename or reuse one.

- **Navigation:** `home`, `direct-messages`, `spaces`, `search`, `explore`, `calls`, `settings`, `notifications`, `notifications-off`
- **Account:** `add-account`, `manage-accounts`, `log-out`
- **Details and panes:** `members`, `thread`, `pin`, `files`, `transfers`, `media`, `details-panel`, `layout`
- **Composer:** `attach`, `send`, `emoji`, `sticker`, `poll`
- **Calls:** `mic`, `mic-off`, `headphones`, `headphones-off`, `camera`, `camera-off`, `screen-share`, `screen-share-stop`, `call-leave`, `chat`
- **Common actions:** `add`, `remove`, `close`, `check`, `refresh`, `download`, `reply`, `react`, `redact`, `edit`, `chevron-up`, `chevron-down`, `more`, `chevrons-left`, `chevrons-right`, `arrow-left`, `arrow-up`, `arrow-down`

## Legacy captions

| Caption | ID |
| --- | --- |
| `⌂` | `home` |
| `S` | `search` |
| `C` | `calls` |
| `P` | `details-panel` |
| `+` | `add` |
| `↓` | `arrow-down` |
| `F` | `files` |
| `↩` | `reply` |
| `♥` | `react` |
| `⌫` | `redact` |
| `✎` | `edit` |
| `Pin` | `pin` |
| `☷` | `thread` |
| `×` | `close` |
| `−` | `remove` |
| `✓` | `check` |
| `⚙` | `settings` |
| `➤` | `send` |
| `✿` | `sticker` |
| `☺` | `emoji` |
| `↻` | `refresh` |
| `✕` | `mic-off` |
| `♩` | `mic` |
| `V` | `camera` |
| `R` | `chat` |
| `⇅` | `transfers` |
| `▲` | `chevron-up` |
| `▼` | `chevron-down` |
| `«` | `chevrons-left` |
| `»` | `chevrons-right` |
| `←` | `arrow-left` |
| `↑` | `arrow-up` |
| `▤` | `poll` |
| `▣` | `layout` |

## Adopting it in another area

Import `MorpheusIcon` (and `MorpheusIcons` if needed) from `ui/icons.slint`, and pass an ID plus your control's current ink. Do not copy paths. If an icon is missing, ask for a new ID to be added here.
- **Room list routes** (navigation-scale worker): `home`, `direct-messages`, `spaces`, `explore`.
- **Room header bell** (notification worker): `notifications`, and `notifications-off` for a muted room.
