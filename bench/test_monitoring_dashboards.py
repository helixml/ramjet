from __future__ import annotations

import importlib.util
import json
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DASHBOARD_DIR = ROOT / "deploy" / "monitoring" / "rtx6000pro"
SYNC = DASHBOARD_DIR / "sync-dashboards.py"
SPEC = importlib.util.spec_from_file_location("sync_dashboards", SYNC)
assert SPEC is not None and SPEC.loader is not None
sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync)

DASHBOARD = DASHBOARD_DIR / "minidynamo-rtx6000pro.json"

# The retired dashboard's identity must not reappear anywhere in the source of
# truth, or the Grafana sidecar would resurrect the old name alongside the new.
RETIRED_MARKERS = ("ds4-flash-serving", "DeepSeek V4 Flash Serving", "Layout Preview")


class DashboardSourceTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.raw = DASHBOARD.read_text(encoding="utf-8")
        cls.document = json.loads(cls.raw)
        cls.panels = cls.document["panels"]

    def test_owned_keys_exist_on_disk(self) -> None:
        for key in sync.OWNED_KEYS:
            self.assertTrue((DASHBOARD_DIR / key).is_file(), key)

    def test_source_is_canonically_formatted(self) -> None:
        # Keeps the mirrored ConfigMap diff limited to real dashboard changes.
        self.assertEqual(self.raw, sync.canonical_json(DASHBOARD))

    def test_identity_is_the_minidynamo_naming(self) -> None:
        self.assertEqual(self.document["title"], "MiniDynamo rtx6000pro")
        self.assertEqual(self.document["uid"], "minidynamo-rtx6000pro")

    def test_no_retired_identity_survives(self) -> None:
        for marker in RETIRED_MARKERS:
            self.assertNotIn(marker, self.raw, marker)

    def test_provisioned_dashboard_carries_no_instance_id(self) -> None:
        # A hard-coded numeric id collides with whatever Grafana already stores.
        self.assertNotIn("id", self.document)

    def test_panel_ids_are_unique(self) -> None:
        ids = [panel["id"] for panel in self.panels]
        self.assertEqual(len(ids), len(set(ids)))

    def test_panels_fit_the_grid_without_overlapping(self) -> None:
        occupied: dict[tuple[int, int], str] = {}
        for panel in self.panels:
            grid = panel["gridPos"]
            self.assertLessEqual(grid["x"] + grid["w"], 24, panel["title"])
            for x in range(grid["x"], grid["x"] + grid["w"]):
                for y in range(grid["y"], grid["y"] + grid["h"]):
                    self.assertNotIn((x, y), occupied, f"{panel['title']} overlaps {occupied.get((x, y))}")
                    occupied[(x, y)] = panel["title"]

    def test_every_target_names_the_prometheus_datasource(self) -> None:
        for panel in self.panels:
            self.assertEqual(panel["datasource"]["uid"], "prometheus", panel["title"])
            for target in panel["targets"]:
                self.assertEqual(target["datasource"]["uid"], "prometheus", panel["title"])

    def _panel(self, title: str) -> dict:
        for panel in self.panels:
            if panel["title"] == title:
                return panel
        raise AssertionError(f"missing panel: {title}")

    def test_readiness_renders_a_parked_engine_as_paused(self) -> None:
        # Regression guard: a readiness panel reading ds4proxy_upstream_up alone
        # shows an idle-drained engine as green READY, hiding the park.
        panel = self._panel("Engine readiness")
        expression = panel["targets"][0]["expr"]
        self.assertIn("ds4proxy_upstream_up", expression)
        self.assertIn("ds4proxy_idle_drain_state", expression)
        mappings = panel["fieldConfig"]["defaults"]["mappings"][0]["options"]
        self.assertEqual(mappings["0"]["text"], "DOWN")
        self.assertEqual(mappings["1"]["text"], "READY")
        self.assertEqual(mappings["2"]["text"], "PAUSED")

    def test_idle_drain_state_panel_is_present(self) -> None:
        panel = self._panel("Idle drain state")
        self.assertEqual(panel["targets"][0]["expr"], "ds4proxy_idle_drain_state")
        self.assertEqual(panel["targets"][0]["legendFormat"], "{{upstream}}")
        states = panel["fieldConfig"]["defaults"]["mappings"][0]["options"]
        self.assertEqual(
            [states[key]["text"] for key in ("0", "1", "2")],
            ["warm", "draining", "drained"],
        )

    def test_readiness_and_drain_share_the_upstream_label(self) -> None:
        # The Grafana panels join these series on `upstream`; diverging legends
        # would silently split a parked engine into two unrelated rows.
        for title in ("Engine readiness", "Idle drain state"):
            self.assertEqual(self._panel(title)["targets"][0]["legendFormat"], "{{upstream}}")


class ConfigMapMirrorTest(unittest.TestCase):
    HEADER = (
        "apiVersion: v1\n"
        "kind: ConfigMap\n"
        "metadata:\n"
        "  name: bunker-dashboards\n"
        "data:\n"
    )

    def configmap(self, *keys: str) -> str:
        body = "".join(f'  {key}: |\n    {{"title": "{key}"}}\n' for key in keys)
        return self.HEADER + body

    def test_unrelated_dashboards_round_trip_byte_for_byte(self) -> None:
        current = self.configmap("bunker-overview.json", "helix-webservices.json")
        updated = sync.build(current, {})
        self.assertEqual(current, updated)

    def test_retired_key_is_dropped_and_owned_key_appended(self) -> None:
        current = self.configmap("bunker-overview.json", "ds4-flash-serving.json")
        updated = sync.build(current, {"minidynamo-rtx6000pro.json": '{\n  "uid": "x"\n}\n'})
        self.assertIn("  bunker-overview.json: |\n", updated)
        self.assertNotIn("ds4-flash-serving.json", updated)
        self.assertIn('  minidynamo-rtx6000pro.json: |\n    {\n      "uid": "x"\n    }\n', updated)

    def test_owned_key_is_refreshed_in_place(self) -> None:
        current = self.configmap("minidynamo-rtx6000pro.json", "bunker-overview.json")
        updated = sync.build(current, {"minidynamo-rtx6000pro.json": '{"uid": "y"}\n'})
        keys = [line for line in updated.splitlines() if line.endswith(": |")]
        self.assertEqual(
            keys, ["  minidynamo-rtx6000pro.json: |", "  bunker-overview.json: |"]
        )
        self.assertIn('    {"uid": "y"}\n', updated)

    def test_sync_is_idempotent(self) -> None:
        dashboards = {key: sync.canonical_json(DASHBOARD_DIR / key) for key in sync.OWNED_KEYS}
        once = sync.build(self.configmap("bunker-overview.json"), dashboards)
        self.assertEqual(once, sync.build(once, dashboards))

    def test_configmap_without_data_mapping_is_rejected(self) -> None:
        with self.assertRaises(sync.SyncError):
            sync.build("apiVersion: v1\nkind: ConfigMap\n", {})

    def test_invalid_dashboard_json_is_rejected(self) -> None:
        with self.assertRaises(sync.SyncError):
            sync.canonical_json(SYNC)


if __name__ == "__main__":
    unittest.main()
