"""Fixed public diagnostics for private disposable Matrix fixture logs."""

from __future__ import annotations

import re


STAGES = frozenset((
    "disk_check", "network", "credentials", "registration", "seed_rooms", "verify",
    "recovery", "rtc_gate", "status", "cleanup",
    "rtc_disk_check", "rtc_network", "rtc_stop_native", "rtc_configs",
    "rtc_federation", "rtc_sfu", "rtc_auth_bridge", "rtc_auth_service",
    "rtc_auth_ready", "rtc_synapse_configure", "rtc_synapse_restart",
    "rtc_synapse_ready", "rtc_versions", "rtc_login", "rtc_transports",
    "rtc_probe_login", "rtc_openid", "rtc_token_exchange", "rtc_sfu_ready",
    *(f"{operation}_{server}" for operation in
      ("volume", "generate", "configure", "start", "ready") for server in ("a", "b")),
))
FAILURES = frozenset((
    "docker", "docker_disk", "docker_permission", "docker_registry", "docker_network",
    "http", "network", "os", "invalid_data", "fixture_operation", "deadline",
))
STAGE = re.compile(r"MORPHEUS_MATRIX_FIXTURE_STAGE ([a-z_]+)")
FAILURE = re.compile(r"MORPHEUS_MATRIX_FIXTURE_FAILURE ([a-z_]+)")
NUMBER = re.compile(r"MORPHEUS_MATRIX_FIXTURE_NUMBER (docker_exit|http_status|os_errno)=([0-9]{1,3})")


def is_progress_line(line: str) -> bool:
    match = STAGE.fullmatch(line)
    return bool(match and match[1] in STAGES)


def matrix_fixture_diagnostics(output: str) -> dict[str, str | int]:
    """Never forward arbitrary exception text, paths, URLs, or response bodies."""
    result: dict[str, str | int] = {}
    for line in output.splitlines():
        if match := STAGE.fullmatch(line):
            if match[1] in STAGES:
                result["stage"] = match[1]
        elif match := FAILURE.fullmatch(line):
            if match[1] in FAILURES:
                result["failure"] = match[1]
        elif match := NUMBER.fullmatch(line):
            key, value = match[1], int(match[2])
            if (key == "http_status" and 100 <= value <= 599 or
                    key != "http_status" and 0 <= value <= 255):
                result[key] = value
    return result
