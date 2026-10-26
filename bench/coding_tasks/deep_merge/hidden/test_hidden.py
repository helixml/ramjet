import copy
import unittest

from layered import deep_merge, merge_all


class HiddenDeepMergeTest(unittest.TestCase):
    def test_lists_replaced_at_depth(self):
        base = {"a": {"b": [1, 2], "c": 1}}
        self.assertEqual(deep_merge(base, {"a": {"b": [3]}}), {"a": {"b": [3], "c": 1}})

    def test_none_deletes(self):
        base = {"a": 1, "b": {"c": 2, "d": 3}}
        self.assertEqual(deep_merge(base, {"a": None}), {"b": {"c": 2, "d": 3}})
        self.assertEqual(deep_merge(base, {"b": {"c": None}}), {"a": 1, "b": {"d": 3}})
        self.assertEqual(deep_merge(base, {"b": None}), {"a": 1})

    def test_none_for_missing_key_is_noop(self):
        self.assertEqual(deep_merge({"a": 1}, {"zzz": None}), {"a": 1})
        self.assertEqual(deep_merge({"a": 1}, {"b": {"c": None}}), {"a": 1, "b": {}})

    def test_new_subtree_drops_none(self):
        result = deep_merge({}, {"x": {"y": None, "z": {"w": None, "v": 1}}})
        self.assertEqual(result, {"x": {"z": {"v": 1}}})

    def test_type_changes(self):
        self.assertEqual(deep_merge({"a": 1}, {"a": {"b": 2}}), {"a": {"b": 2}})
        self.assertEqual(deep_merge({"a": {"b": 2}}, {"a": 5}), {"a": 5})
        self.assertEqual(deep_merge({"a": {"b": 2}}, {"a": [1]}), {"a": [1]})

    def test_falsy_values_are_not_deletions(self):
        result = deep_merge({"a": 1, "b": 2, "c": 3, "d": 4}, {"a": 0, "b": "", "c": [], "d": False})
        self.assertEqual(result, {"a": 0, "b": "", "c": [], "d": False})
        self.assertEqual(deep_merge({"a": {"x": 1}}, {"a": {}}), {"a": {"x": 1}})

    def test_inputs_not_mutated(self):
        base = {"a": {"b": [1, 2], "c": {"d": 1}}, "e": 1}
        override = {"a": {"b": [3], "c": {"d": None, "f": [9]}}, "e": None, "g": {"h": None}}
        base_before, override_before = copy.deepcopy(base), copy.deepcopy(override)
        deep_merge(base, override)
        self.assertEqual(base, base_before)
        self.assertEqual(override, override_before)

    def test_result_does_not_alias_inputs(self):
        base = {"keep": {"list": [1]}, "nested": {"x": 1}}
        override = {"new": {"list": [2]}, "replaced": [3]}
        result = deep_merge(base, override)
        result["keep"]["list"].append(99)
        result["nested"]["y"] = 2
        result["new"]["list"].append(99)
        result["replaced"].append(99)
        self.assertEqual(base, {"keep": {"list": [1]}, "nested": {"x": 1}})
        self.assertEqual(override, {"new": {"list": [2]}, "replaced": [3]})

    def test_merge_all(self):
        defaults = {"log": {"level": "info", "sinks": ["stdout"]}, "workers": 4}
        prod = {"log": {"level": "warning"}, "workers": 16}
        local = {"log": {"sinks": ["file"]}, "workers": None}
        snapshot = copy.deepcopy([defaults, prod, local])
        self.assertEqual(
            merge_all(defaults, prod, local),
            {"log": {"level": "warning", "sinks": ["file"]}},
        )
        self.assertEqual([defaults, prod, local], snapshot)
        self.assertEqual(merge_all(), {})
        single = merge_all(defaults)
        self.assertEqual(single, defaults)
        single["log"]["level"] = "debug"
        self.assertEqual(defaults["log"]["level"], "info")


if __name__ == "__main__":
    unittest.main()
