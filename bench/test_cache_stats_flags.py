"""Every model deployment must report prefix-cache hits in its API responses.

On 2026-08-27 the Qwen3.8-Flash-Next API was found to return no cache-hit
statistics at all. The cause was not the model, the runtime, or ramjet: vLLM's
``--enable-prompt-tokens-details`` defaults to ``False`` and the deployment
never passed it, so ``prompt_tokens_details`` was omitted from every response
while the native prefix cache was serving ~66% of queried blocks.

The flag was missing because the deployment's serving command was written from
the upstream vLLM recipe, and no upstream recipe mentions it. The older DSpark
stack was unaffected only because its baked entrypoint had carried the flag all
along. That is exactly the kind of gap that reappears the next time a
deployment is added by copying a recipe, so it is checked here rather than
rediscovered from a user report.

The check is deliberately engine-aware and location-agnostic. A deployment's
serving arguments legitimately live in a Compose ``command``, in a launch
script, or baked into an image entrypoint and recorded in a pinned runtime
manifest -- grepping only the Compose files is what made the first audit of
this gap wrong in both directions.
"""

import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy"

VLLM_FLAG = "--enable-prompt-tokens-details"
SGLANG_FLAG = "--enable-cache-report"

# Deployments that serve a model, and the engine whose flag spelling applies.
# Adding a GPU deployment without registering it here fails
# ``test_every_gpu_deployment_is_registered``, which is the point: the
# registration is where somebody has to decide which flag the engine needs.
DEPLOYMENTS = {
    "qwen38_flash_next": "vllm",
    "dspark_0731": "vllm",
    "qwen38_27b": "sglang",
    "glm53_flash": "sglang",
}

EXPECTED_FLAG = {"vllm": VLLM_FLAG, "sglang": SGLANG_FLAG}

# Suffixes that can carry serving arguments. ``.json`` covers pinned runtime
# manifests, whose recorded argv is authoritative for an image-baked launcher.
ARGV_SUFFIXES = (".yaml", ".yml", ".sh", ".json")


def _argv_text(directory: pathlib.Path) -> str:
    """Concatenate everything in a deployment that can carry serving argv.

    Pinned runtime manifests store argv as a JSON list, so their elements are
    joined explicitly: a bare substring search over the raw file would also
    match prose in an unrelated description field.
    """
    chunks = []
    for path in sorted(directory.rglob("*")):
        if not path.is_file() or path.suffix not in ARGV_SUFFIXES:
            continue
        if "__pycache__" in path.parts:
            continue
        try:
            raw = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if path.suffix == ".json":
            try:
                chunks.append(" ".join(_json_argv(json.loads(raw))))
            except (json.JSONDecodeError, TypeError):
                continue
        else:
            chunks.append(raw)
    return "\n".join(chunks)


def _json_argv(document: object) -> list[str]:
    """Yield every string in any ``argv`` list found in a runtime manifest."""
    found: list[str] = []
    if isinstance(document, dict):
        for key, value in document.items():
            if key == "argv" and isinstance(value, list):
                found.extend(str(item) for item in value)
            else:
                found.extend(_json_argv(value))
    elif isinstance(document, list):
        for item in document:
            found.extend(_json_argv(item))
    return found


def _gpu_deployments() -> set[str]:
    """Deployment directories that reserve NVIDIA GPUs, i.e. serve a model."""
    found = set()
    for path in DEPLOY.rglob("*.yaml"):
        if "__pycache__" in path.parts:
            continue
        try:
            raw = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if "driver: nvidia" in raw:
            found.add(path.relative_to(DEPLOY).parts[0])
    return found


class CacheStatsFlagTests(unittest.TestCase):
    def test_every_deployment_reports_cache_hits(self):
        for name, engine in sorted(DEPLOYMENTS.items()):
            with self.subTest(deployment=name, engine=engine):
                directory = DEPLOY / name
                self.assertTrue(
                    directory.is_dir(), f"{name} is registered but absent"
                )
                # assertTrue, not assertIn: the haystack is every serving
                # file in the deployment and unittest would print all of it.
                self.assertTrue(
                    EXPECTED_FLAG[engine] in _argv_text(directory),
                    f"{name} ({engine}) never passes {EXPECTED_FLAG[engine]}, so "
                    "its API returns no prefix-cache statistics",
                )

    def test_engine_flag_spellings_are_not_crossed(self):
        """A vLLM deployment carrying the SGLang flag (or vice versa) is inert.

        Both engines accept unknown arguments differently and a crossed flag
        would look present to a careless grep while reporting nothing.
        """
        for name, engine in sorted(DEPLOYMENTS.items()):
            with self.subTest(deployment=name, engine=engine):
                wrong = SGLANG_FLAG if engine == "vllm" else VLLM_FLAG
                self.assertFalse(
                    wrong in _argv_text(DEPLOY / name),
                    f"{name} is {engine} but carries {wrong}",
                )

    def test_every_gpu_deployment_is_registered(self):
        self.assertEqual(
            _gpu_deployments(),
            set(DEPLOYMENTS),
            "a GPU deployment is not registered above; decide which cache-stats "
            "flag its engine needs and add it to DEPLOYMENTS",
        )


if __name__ == "__main__":
    unittest.main()
