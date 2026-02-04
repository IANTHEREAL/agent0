from __future__ import annotations


def escape_like_pattern(pattern: str) -> str:
    # Upstream (dify@acfd34e8767c3f7c887f99c51763f6150ff21898):
    # api/libs/helper.py#L35
    if not pattern:
        return pattern
    return pattern.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")


def convert_datetime_to_date(field: str, target_timezone: str = ":tz", db_type: str = "postgresql") -> str:
    # Upstream (dify@acfd34e8767c3f7c887f99c51763f6150ff21898):
    # api/libs/helper.py#L230
    if db_type == "postgresql":
        return f"DATE(DATE_TRUNC('day', {field} AT TIME ZONE 'UTC' AT TIME ZONE {target_timezone}))"
    raise NotImplementedError(f"Unsupported database type: {db_type}")

