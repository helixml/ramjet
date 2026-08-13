#!/usr/bin/env python3
"""Fail closed if pinned benchmark source contracts drift from the harness."""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess


NVBANDWIDTH_SHA = "82fc4e8c6afa0babb8687793678f615b3b8d793e"
NCCL_TESTS_SHA = "717b68318278e93f371d8ffb46b076069d7c7851"


def read(root: pathlib.Path, relative: str) -> str:
    return (root / relative).read_text(encoding="utf-8")


def require(text: str, pattern: str, description: str) -> None:
    if not re.search(pattern, text, re.S):
        raise SystemExit(f"pinned source contract missing: {description}")


def require_commit(root: pathlib.Path, expected: str) -> None:
    actual = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    if actual != expected:
        raise SystemExit(f"pinned source identity mismatch: {root.name}")


def verify_nvbandwidth(root: pathlib.Path) -> None:
    require_commit(root, NVBANDWIDTH_SHA)
    cmake = read(root, "CMakeLists.txt")
    cli = read(root, "nvbandwidth.cpp")
    cases = read(root, "testcases.cpp")
    output = read(root, "json_output.cpp")
    require(cmake, r"project\(nvbandwidth\s+VERSION\s+0\.10\.0", "version 0.10.0")
    require(
        cli,
        r'add_argument\("-t",\s*"--testcase"\).*?'
        r"\.nargs\(argparse::nargs_pattern::any\).*?\.append\(\)",
        "one -t accepting multiple testcase names",
    )
    require(
        cli,
        r'add_argument\("-F",\s*"--format"\).*?'
        r'\.choices\("text",\s*"json",\s*"perf"\)',
        "-F json output selection",
    )
    for fragment in (
        'std::string result = "device_to_device_memcpy_"',
        'result += (accessType == AccessType::Read) ? "read_" : "write_"',
        'case CopyInitiator::CE:  result += "ce"',
        'case CopyInitiator::SM:  result += "sm"',
        'Testcase("device_to_device_latency_sm"',
    ):
        if fragment not in cases + read(root, "testcase.h"):
            raise SystemExit(f"pinned source contract missing: {fragment}")
    for key, value in (
        ("NVB_TITLE", "nvbandwidth"),
        ("NVB_TESTCASES", "testcases"),
        ("NVB_TESTCASE_NAME", "name"),
        ("NVB_STATUS", "status"),
        ("NVB_BW_MATRIX", "bandwidth_matrix"),
        ("NVB_PASSED", "Passed"),
    ):
        require(
            output,
            rf'const std::string {key}\("{value}"\)',
            f"nvbandwidth JSON field {value}",
        )


def verify_nccl_tests(root: pathlib.Path) -> None:
    require_commit(root, NCCL_TESTS_SHA)
    source = read(root, "src/common.cu")
    for long_name, short_name in (
        ("iters", "n"),
        ("warmup_iters", "w"),
        ("check", "c"),
        ("timeout", "T"),
    ):
        require(
            source,
            rf'\{{"{long_name}",\s*required_argument,\s*0,\s*\'{short_name}\'\}}',
            f"nccl-tests --{long_name}",
        )
    require(source, r"if \(errors\[0\].*return testNumResults", "nonzero data errors")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("nvbandwidth", type=pathlib.Path)
    parser.add_argument("nccl_tests", type=pathlib.Path)
    args = parser.parse_args()
    verify_nvbandwidth(args.nvbandwidth)
    verify_nccl_tests(args.nccl_tests)
    print("pinned benchmark source contracts verified")


if __name__ == "__main__":
    main()
