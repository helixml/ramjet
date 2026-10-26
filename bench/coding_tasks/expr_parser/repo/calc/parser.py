"""A small arithmetic expression evaluator."""

import operator
import re


class ParseError(ValueError):
    """The expression is not well formed."""


TOKEN = re.compile(r"\s*(?:(\d+\.\d*|\.\d+|\d+)|(\*\*|[-+*/()]))")

OPERATORS = {
    "+": operator.add,
    "-": operator.sub,
    "*": operator.mul,
    "/": operator.truediv,
    "**": operator.pow,
}


def tokenize(text):
    tokens = []
    text = text.rstrip()
    position = 0
    while position < len(text):
        match = TOKEN.match(text, position)
        if not match:
            raise ParseError(f"unexpected character at offset {position}")
        number, symbol = match.groups()
        if number is not None:
            tokens.append(("num", float(number) if "." in number else int(number)))
        else:
            tokens.append(("op", symbol))
        position = match.end()
    return tokens


class Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.position = 0

    def peek(self):
        if self.position < len(self.tokens):
            return self.tokens[self.position]
        return None

    def take(self):
        token = self.peek()
        if token is not None:
            self.position += 1
        return token

    def parse(self):
        value = self.expression()
        if self.peek() is not None:
            raise ParseError("unexpected trailing input")
        return value

    def expression(self):
        value = self.atom()
        while self.peek() is not None and self.peek()[1] in OPERATORS:
            symbol = self.take()[1]
            value = OPERATORS[symbol](value, self.atom())
        return value

    def atom(self):
        token = self.take()
        if token is None:
            raise ParseError("unexpected end of input")
        kind, value = token
        if kind == "num":
            return value
        if value == "(":
            inner = self.expression()
            if self.take() != ("op", ")"):
                raise ParseError("expected ')'")
            return inner
        raise ParseError(f"unexpected {value!r}")


def evaluate(text):
    return Parser(tokenize(text)).parse()
