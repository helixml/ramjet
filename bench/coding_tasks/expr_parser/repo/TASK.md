# Calculator gets operator precedence wrong

`calc.evaluate("1 + 2 * 3")` returns `9`; it should be `7`. While looking into
it we found more problems in `calc/parser.py`:

- `*` and `/` must bind tighter than `+` and `-`; all four are
  left-associative (`8 - 3 - 2 == 3`, `16 / 4 / 2 == 2`).
- `**` (power) binds tighter than `*` and `/` and is **right**-associative:
  `2 ** 3 ** 2 == 512`.
- Unary minus and unary plus are not supported at all (`-3`, `2 * -3`,
  `-(1 + 2)`, `--3`). They should follow Python's rules: unary minus binds
  more loosely than `**` on its right-hand side, so `-2 ** 2 == -4`, but an
  exponent may itself be negative: `2 ** -1 == 0.5`.

Other requirements:

- Numbers are non-negative integer or decimal literals (`3`, `2.5`, `.5`).
  Integer-only arithmetic with `+ - * **` should produce `int` results;
  `/` is true division.
- Whitespace is insignificant.
- Malformed input (empty string, dangling operator, unbalanced parentheses,
  two numbers in a row, unknown characters) must raise `calc.ParseError`
  (a subclass of `ValueError`). Division by zero raises `ZeroDivisionError`.

Keep the public API: `calc.evaluate(text)` and `calc.ParseError`.
