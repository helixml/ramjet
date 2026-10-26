"""A small arithmetic expression evaluator."""

import operator
import re


class ParseError(ValueError):
    """The expression is not well formed."""


TOKEN = re.compile(r"\s*(?:(\d+\.\d*|\.\d+|\d+)|(\*\*|[-+*/()]))")

ADDITIVE = {"+": operator.add, "-": operator.sub}
MULTIPLICATIVE = {"*": operator.mul, "/": operator.truediv}


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

    def peek_op(self, table):
        token = self.peek()
        return token is not None and token[0] == "op" and token[1] in table

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
        value = self.term()
        while self.peek_op(ADDITIVE):
            value = ADDITIVE[self.take()[1]](value, self.term())
        return value

    def term(self):
        value = self.unary()
        while self.peek_op(MULTIPLICATIVE):
            value = MULTIPLICATIVE[self.take()[1]](value, self.unary())
        return value

    def unary(self):
        if self.peek_op(("-", "+")):
            sign = self.take()[1]
            value = self.unary()
            return -value if sign == "-" else +value
        return self.power()

    def power(self):
        base = self.atom()
        if self.peek_op(("**",)):
            self.take()
            return base ** self.unary()
        return base

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
