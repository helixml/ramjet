import unittest

from calc import ParseError, evaluate


class HiddenEvaluateTest(unittest.TestCase):
    def check(self, text, expected):
        result = evaluate(text)
        self.assertEqual(result, expected, text)
        self.assertIs(type(result), type(expected), text)

    def test_precedence(self):
        self.check("1 + 2 * 3", 7)
        self.check("2 * 3 + 4 * 5", 26)
        self.check("10 - 4 / 2", 8.0)
        self.check("2 * 3 ** 2", 18)

    def test_left_associativity(self):
        self.check("8 - 3 - 2", 3)
        self.check("16 / 4 / 2", 2.0)
        self.check("2 * 6 / 3", 4.0)

    def test_power_right_associative(self):
        self.check("2 ** 3 ** 2", 512)
        self.check("(2 ** 3) ** 2", 64)

    def test_unary(self):
        self.check("-3", -3)
        self.check("+3", 3)
        self.check("2 * -3", -6)
        self.check("-(1 + 2)", -3)
        self.check("--3", 3)
        self.check("1 - -1", 2)
        self.check("-2 ** 2", -4)
        self.check("(-2) ** 2", 4)
        self.check("2 ** -1", 0.5)
        self.check("-2 ** -2", -0.25)

    def test_floats_and_whitespace(self):
        self.check(".5 + 1", 1.5)
        self.check("  7/2", 3.5)
        self.check("1.0*2", 2.0)
        self.check("((((1))))", 1)

    def test_division_by_zero(self):
        with self.assertRaises(ZeroDivisionError):
            evaluate("1 / (2 - 2)")

    def test_malformed(self):
        for text in ("", "   ", "1 +", "(1", "1)", "2 3", "1 $ 2", "* 2", "()", "2 ** ", "-"):
            with self.subTest(text=text):
                with self.assertRaises(ParseError):
                    evaluate(text)

    def test_parse_error_is_value_error(self):
        self.assertTrue(issubclass(ParseError, ValueError))


if __name__ == "__main__":
    unittest.main()
