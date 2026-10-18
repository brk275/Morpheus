"""Closed timeout diagnostics; never forward fixture values or failed assertions."""

STAGES = frozenset((
    "catalog", "consent", "room_preview", "room_enqueue", "room_outcome",
    "room_receive", "room_queue_clear", "thread_catalog", "thread_send",
    "thread_receive", "thread_queue_clear", "barrier", "pack_change",
    "enabled_packs", "plain_room", "disable_pack", "recovery_gate",
    "gate_send", "departure", "complete",
))
FAILURES = frozenset(("scenario_deadline", "check_failed"))
CLEANUP = frozenset(("failed",))
PREFIXES = (
    ("MORPHEUS_STICKER_STAGE ", "stage", STAGES),
    ("MORPHEUS_STICKER_FAILURE ", "failure", FAILURES),
    ("MORPHEUS_STICKER_CLEANUP_FAILURE ", "cleanup", CLEANUP),
)


def sticker_diagnostics(output: str) -> dict:
    """Expose a last await stage only for a confirmed scenario deadline.

    A failed assertion could concern a privacy boundary. Its message and
    stage remain withheld rather than publishing vulnerability details.
    Markers are diagnostic hints, never evidence that the test passed.
    The caller scans its bounded private log before invoking this parser.
    """
    found = {}
    invalid = False
    seen = False
    for line in output.splitlines():
        for prefix, key, allowed in PREFIXES:
            if not line.startswith(prefix):
                continue
            seen = True
            # Bound before slicing; never keep an oversized arbitrary value.
            if len(line) > len(prefix) + 80:
                invalid = True
                break
            value = line[len(prefix):]
            if value in allowed:
                found[key] = value
            else:
                invalid = True
            break
    if not seen:
        return {}
    result = {
        "failure": found.get("failure", "unknown"),
        "stage": (found.get("stage", "not_started")
                  if found.get("failure") == "scenario_deadline" else "withheld"),
        "invalid_marker": invalid,
    }
    if "cleanup" in found:
        result["cleanup"] = found["cleanup"]
    return result
