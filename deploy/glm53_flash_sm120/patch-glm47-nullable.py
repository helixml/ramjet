#!/usr/bin/env python3
"""Patch the pinned GLM47 streaming parser to preserve nullable strings."""

from __future__ import annotations

import hashlib
from pathlib import Path


PARSER = Path(
    "/opt/sglang-source/python/sglang/srt/function_call/glm47_moe_detector.py"
)
EXPECTED_SHA256 = "4de49ef0ea1ced956e95b1ad7cfabee5823731230cb5549dc05caff49ec0bcf2"


def replace_once(source: str, old: str, new: str) -> str:
    if source.count(old) != 1:
        raise SystemExit("pinned GLM47 parser shape changed")
    return source.replace(old, new, 1)


def patch_source(source: str) -> str:
    source = replace_once(
        source,
        """        arg_type = get_argument_type(func_name, key, tools)
        if arg_type:
            return arg_type
""",
        """        name2tool = {tool.function.name: tool for tool in tools}
        tool = name2tool.get(func_name)
        params = getattr(tool.function, "parameters", None) if tool else None
        arg_spec = get_schema_properties(params).get(key)
        declared_types = arg_spec.get("type") if isinstance(arg_spec, dict) else None
        if (
            isinstance(declared_types, list)
            and set(declared_types) == {"string", "null"}
        ):
            # A streaming value cannot be emitted as a string until we know
            # whether the complete XML value is the JSON null literal.
            return "nullable_string"

        arg_type = get_argument_type(func_name, key, tools)
        if arg_type:
            return arg_type
""",
    )

    source = replace_once(
        source,
        """        if value_type == "string":
            # Ensure proper JSON string formatting with quotes
            return json.dumps(value, ensure_ascii=False)
""",
        """        if value_type == "nullable_string":
            return "null" if value.strip() == "null" else json.dumps(
                value, ensure_ascii=False
            )
        if value_type == "string":
            # Ensure proper JSON string formatting with quotes
            return json.dumps(value, ensure_ascii=False)
""",
    )

    source = replace_once(
        source,
        """                        if value_type == "string":
                            if not self._value_started:
""",
        """                        if value_type == "nullable_string":
                            # Buffer the whole value. Emitting any byte now
                            # would force either quoted-string or JSON-null
                            # syntax before the closing tag disambiguates it.
                            if content:
                                self._current_value += content
                                self._xml_tag_buffer = ""
                        elif value_type == "string":
                            if not self._value_started:
""",
    )
    return source


def main() -> None:
    raw = PARSER.read_bytes()
    if hashlib.sha256(raw).hexdigest() != EXPECTED_SHA256:
        raise SystemExit("pinned GLM47 parser digest changed")
    PARSER.write_text(patch_source(raw.decode("utf-8")), encoding="utf-8")


if __name__ == "__main__":
    main()
