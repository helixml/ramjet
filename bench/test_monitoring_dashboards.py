from __future__ import annotations

import importlib.util
import json
import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
METRICS_SOURCE = ROOT / "src" / "metrics.rs"
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


def visualizations(panels: list[dict]) -> list[dict]:
    """Every panel that draws something, including panels nested in a row."""
    out: list[dict] = []
    for panel in panels:
        if panel["type"] == "row":
            out.extend(panel.get("panels", []))
        else:
            out.append(panel)
    return out


class DashboardSourceTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.raw = DASHBOARD.read_text(encoding="utf-8")
        cls.document = json.loads(cls.raw)
        cls.panels = visualizations(cls.document["panels"])

    def test_owned_keys_exist_on_disk(self) -> None:
        for key in sync.OWNED_KEYS:
            self.assertTrue((DASHBOARD_DIR / key).is_file(), key)

    def test_source_is_canonically_formatted(self) -> None:
        # Keeps the mirrored ConfigMap diff limited to real dashboard changes.
        self.assertEqual(self.raw, sync.canonical_json(DASHBOARD))

    def test_identity_is_the_minidynamo_naming(self) -> None:
        self.assertEqual(self.document["title"], "Ramjet rtx6000pro")
        self.assertEqual(self.document["uid"], "minidynamo-rtx6000pro")

    def test_no_retired_identity_survives(self) -> None:
        for marker in RETIRED_MARKERS:
            self.assertNotIn(marker, self.raw, marker)

    def test_provisioned_dashboard_carries_no_instance_id(self) -> None:
        # A hard-coded numeric id collides with whatever Grafana already stores.
        self.assertNotIn("id", self.document)

    def test_panel_ids_are_unique(self) -> None:
        # Rows share the id space with the panels they contain.
        ids = [panel["id"] for panel in self.panels]
        ids.extend(row["id"] for row in self.document["panels"] if row["type"] == "row")
        self.assertEqual(len(ids), len(set(ids)))

    def _assert_laid_out(self, panels: list[dict]) -> None:
        occupied: dict[tuple[int, int], str] = {}
        for panel in panels:
            grid = panel["gridPos"]
            self.assertLessEqual(grid["x"] + grid["w"], 24, panel["title"])
            for x in range(grid["x"], grid["x"] + grid["w"]):
                for y in range(grid["y"], grid["y"] + grid["h"]):
                    self.assertNotIn((x, y), occupied, f"{panel['title']} overlaps {occupied.get((x, y))}")
                    occupied[(x, y)] = panel["title"]

    def test_panels_fit_the_grid_without_overlapping(self) -> None:
        # A row's children are laid out in their own space, below the row header.
        self._assert_laid_out([p for p in self.document["panels"] if p["type"] != "row"])
        for row in self.document["panels"]:
            if row["type"] == "row":
                self._assert_laid_out(row.get("panels", []))

    def test_rows_sit_below_the_always_visible_panels(self) -> None:
        rows = [p for p in self.document["panels"] if p["type"] == "row"]
        self.assertTrue(rows, "expected the idle-drain row")
        highest_row = min(row["gridPos"]["y"] for row in rows)
        for panel in self.document["panels"]:
            if panel["type"] == "row":
                continue
            bottom = panel["gridPos"]["y"] + panel["gridPos"]["h"]
            self.assertLessEqual(bottom, highest_row, panel["title"])

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
        # Regression guard: a readiness panel reading ramjet_upstream_up alone
        # shows an idle-drained engine as green READY, hiding the park.
        panel = self._panel("Engine readiness")
        expression = panel["targets"][0]["expr"]
        self.assertIn("ramjet_upstream_up", expression)
        self.assertIn("ramjet_idle_drain_state", expression)
        mappings = panel["fieldConfig"]["defaults"]["mappings"][0]["options"]
        self.assertEqual(mappings["0"]["text"], "DOWN")
        self.assertEqual(mappings["1"]["text"], "READY")
        self.assertEqual(mappings["2"]["text"], "PAUSED")

    def test_idle_drain_state_panel_is_present(self) -> None:
        panel = self._panel("Idle drain state")
        self.assertEqual(panel["targets"][0]["expr"], "ramjet_idle_drain_state")
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

    def test_readiness_survives_the_policy_being_off(self) -> None:
        # ramjet_idle_drain_state is only exported while the policy runs, and
        # it is off in the canonical deployment. Without the `or` fallback the
        # whole readiness tile evaluates empty, so the panel would go blank
        # rather than degrade to DOWN/READY.
        expression = self._panel("Engine readiness")["targets"][0]["expr"]
        self.assertIn("or (ramjet_upstream_up * 0)", expression)
        # A down engine must read DOWN even while it is drained, which is why
        # health multiplies the drain term instead of adding to it.
        self.assertIn("ramjet_upstream_up * (1 +", expression)

    def test_stop_intent_panel_shows_both_converger_inputs(self) -> None:
        # The privileged actor stops an engine only when desired running is
        # false AND safe to stop is true; either alone is not an instruction.
        panel = self._panel("Stop intent (desired running / safe to stop)")
        self.assertEqual(
            [target["expr"] for target in panel["targets"]],
            ["ramjet_idle_drain_desired_running", "ramjet_idle_drain_safe_to_stop"],
        )
        mappings = panel["fieldConfig"]["defaults"]["mappings"][0]["options"]
        self.assertEqual([mappings[key]["text"] for key in ("0", "1")], ["no", "yes"])

    def test_fleet_idle_window_panel_is_present(self) -> None:
        panel = self._panel("Fleet idle window")
        self.assertEqual(panel["targets"][0]["expr"], "ramjet_idle_drain_fleet_idle")
        mappings = panel["fieldConfig"]["defaults"]["mappings"][0]["options"]
        self.assertEqual([mappings[key]["text"] for key in ("0", "1")], ["serving", "idle"])

    def test_transitions_panel_reports_a_rate_not_a_raw_counter(self) -> None:
        # Raw ramjet_idle_drain_transitions_total only ever climbs; the
        # flapping this panel exists to catch is visible in the rate.
        panel = self._panel("Drain transitions (per hour)")
        expression = panel["targets"][0]["expr"]
        self.assertIn("rate(ramjet_idle_drain_transitions_total", expression)
        self.assertIn("sum by (upstream, state)", expression)
        self.assertIn("* 3600", expression)

    def test_idle_drain_panels_explain_an_empty_result(self) -> None:
        # The policy is off in the canonical deployment, so these panels are
        # normally empty; without noValue they read as broken rather than idle.
        for title in (
            "Idle drain state",
            "Stop intent (desired running / safe to stop)",
            "Fleet idle window",
            "Drain transitions (per hour)",
        ):
            defaults = self._panel(title)["fieldConfig"]["defaults"]
            self.assertEqual(defaults.get("noValue"), "idle-drain policy is off", title)

    def test_idle_drain_panels_are_grouped_in_one_row(self) -> None:
        rows = [p for p in self.document["panels"] if p["type"] == "row"]
        self.assertEqual([row["title"] for row in rows], ["Idle drain (idle power parking)"])
        row = rows[0]
        self.assertTrue(row["collapsed"], "the policy is off by default; keep the row folded")
        self.assertEqual(
            [panel["title"] for panel in row["panels"]],
            [
                "Idle drain state",
                "Stop intent (desired running / safe to stop)",
                "Fleet idle window",
                "Drain transitions (per hour)",
                "Release pressure (mean in-flight per serving replica)",
                "Engine sleep state",
                "Sleep/wake actuations (per hour)",
                "Sleep/wake duration (p95)",
            ],
        )

    def test_readiness_tile_shows_a_state_not_a_sparkline(self) -> None:
        # graphMode "area" draws a sparkline of the 0/1/2 state code, which
        # renders a DOWN -> READY recovery as a meaningless ramp.
        panel = self._panel("Engine readiness")
        self.assertEqual(panel["options"]["graphMode"], "none")


def exported_metrics() -> dict[str, tuple[str, ...]]:
    """Map each `ramjet_` metric declared in src/metrics.rs to its labels.

    The registrations are `name, help, &[labels]` triples, so the label list is
    whatever `&[...]` appears before the constructor's closing `)?`. Metrics
    built from a plain `Gauge` carry no labels and yield an empty tuple.
    """
    source = METRICS_SOURCE.read_text(encoding="utf-8")
    declared: dict[str, tuple[str, ...]] = {}
    for match in re.finditer(r'"(ramjet_[a-z0-9_]+)"', source):
        name = match.group(1)
        if name in declared:
            continue  # A later mention is a test or a registration, not a new metric.
        tail = source[match.end() : match.end() + 400]
        end = tail.find(")?")
        labels = re.search(r"&\[([^\]]*)\]", tail if end == -1 else tail[:end])
        declared[name] = tuple(re.findall(r'"([a-z_]+)"', labels.group(1))) if labels else ()
    return declared


def referenced_metrics(panels: list[dict]) -> set[str]:
    names: set[str] = set()
    for panel in visualizations(panels):
        for target in panel["targets"]:
            names.update(re.findall(r"\bramjet_[a-z0-9_]+", target["expr"]))
    return names


class MetricContractTest(unittest.TestCase):
    """Ties the dashboard's queries to the metrics the LB actually exports.

    Without this, renaming a metric in Rust leaves a silently empty panel: the
    query stays valid PromQL and Grafana just renders "No data".
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls.declared = exported_metrics()
        cls.document = json.loads(DASHBOARD.read_text(encoding="utf-8"))
        cls.referenced = referenced_metrics(cls.document["panels"])

    def base_name(self, metric: str) -> str:
        # Histograms are registered under their base name and queried per suffix.
        for suffix in ("_bucket", "_sum", "_count"):
            if metric.endswith(suffix) and metric[: -len(suffix)] in self.declared:
                return metric[: -len(suffix)]
        return metric

    def test_every_queried_metric_is_exported(self) -> None:
        self.assertTrue(self.referenced, "expected ramjet queries on the dashboard")
        undefined = sorted(m for m in self.referenced if self.base_name(m) not in self.declared)
        self.assertEqual(undefined, [], "queried but never exported")

    def test_all_idle_drain_metrics_are_surfaced(self) -> None:
        # The dashboard previously showed 1 of the 5; the drain state alone does
        # not tell an operator whether stopping an engine is safe or why.
        drain = {name for name in self.declared if "idle_drain" in name}
        self.assertEqual(sorted(drain - self.referenced), [])

    def test_all_engine_park_metrics_are_surfaced(self) -> None:
        # Same reasoning as the drain metrics, and the gap was real: the park
        # gauge, actuation counter, and duration histogram all shipped exported
        # but invisible, so an operator could see that a replica was withheld
        # without seeing that the balancer had put the engine to sleep.
        park = {name for name in self.declared if "engine_park" in name}
        # A histogram is declared under its base name but queried per suffix,
        # so compare against the base names the dashboard actually reaches.
        surfaced = {self.base_name(name) for name in self.referenced}
        self.assertEqual(sorted(park - surfaced), [])

    def test_drain_and_readiness_labels_agree(self) -> None:
        # The readiness panel multiplies these two series together, which in
        # PromQL requires identical label sets, not merely a shared `upstream`.
        self.assertEqual(
            self.declared["ramjet_idle_drain_state"],
            self.declared["ramjet_upstream_up"],
        )

    def test_transitions_are_grouped_by_their_real_labels(self) -> None:
        self.assertEqual(
            self.declared["ramjet_idle_drain_transitions_total"], ("upstream", "state")
        )

    def test_fleet_idle_is_unlabelled(self) -> None:
        # It is fleet-wide, so a {{upstream}} legend on it would render empty.
        self.assertEqual(self.declared["ramjet_idle_drain_fleet_idle"], ())
        panel = next(
            p
            for p in visualizations(self.document["panels"])
            if p["title"] == "Fleet idle window"
        )
        self.assertNotIn("{{", panel["targets"][0]["legendFormat"])


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
