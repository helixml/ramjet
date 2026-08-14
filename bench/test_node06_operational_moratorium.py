import unittest
from unittest import mock

import node06_operational_moratorium as moratorium


class Node06OperationalMoratoriumTests(unittest.TestCase):
    def test_active_moratorium_fails_closed_with_bounded_reason(self):
        with self.assertRaisesRegex(moratorium.MoratoriumError, "moratorium"):
            moratorium.require_active_work_permitted("gpu-workload.focused-smoke")
        self.assertEqual(
            moratorium.MORATORIUM_REASON,
            "cooling_ac_failure_2026_08_14",
        )

    def test_only_reviewed_inactive_state_permits_bounded_operation(self):
        with mock.patch.object(moratorium, "MORATORIUM_ACTIVE", False):
            self.assertIsNone(
                moratorium.require_active_work_permitted(
                    "p2p-full-prerequisite"
                )
            )

    def test_operation_name_is_bounded_even_after_lift(self):
        with mock.patch.object(moratorium, "MORATORIUM_ACTIVE", False):
            for operation in ("", "UPPER", "x" * 97, "gpu workload"):
                with self.subTest(operation=operation), self.assertRaises(
                    moratorium.MoratoriumError
                ):
                    moratorium.require_active_work_permitted(operation)
