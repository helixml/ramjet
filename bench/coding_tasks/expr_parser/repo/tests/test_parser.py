import unittest

from calc import ParseError, evaluate


class EvaluateTest(unittest.TestCase):
    def test_numbers(self):
        self.assertEqual(evaluate("42"), 42)
        self.assertEqual(evaluate(" 2.5 "), 2.5)

    def test_parentheses(self):
        self.assertEqual(evaluate("(1 + 2) * 3"), 9)

    def test_precedence(self):
        self.assertEqual(evaluate("1 + 2 * 3"), 7)

    def test_malformed(self):
        with self.assertRaises(ParseError):
            evaluate("1 +")


if __name__ == "__main__":
    unittest.main()
