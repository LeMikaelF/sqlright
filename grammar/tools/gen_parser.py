#!/usr/bin/env python3
"""Generate the Rust parser for a SQLRight Bison grammar.

The C++ SQLRight parser is a Bison grammar whose semantic actions build the
SQLRight IR. This tool keeps the grammar as the single source of truth and
produces Rust from it:

  1. It parses the grammar file: declarations, rules, and actions.
  2. It writes a copy of the grammar without actions and runs `bison` on it.
     Bison computes the LALR(1) tables, which the Rust parser driver uses.
  3. It translates the C++ actions to Rust. Almost all actions follow a small
     set of patterns. The few irregular ones are translated by extra rules in
     `translate_statement`.
  4. It writes the IR type enums from `define.h`.

Usage:
  gen_parser.py --grammar-dir grammar/postgres \
                --parser-out crates/sqlright-pg-parser/src/generated \
                --ir-out crates/sqlright-ir/src/generated

The generated files are committed. Run the tool again after a grammar change.
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

# ---------------------------------------------------------------------------
# Grammar file parsing
# ---------------------------------------------------------------------------


@dataclass
class Alternative:
    symbols: List[str]  # grammar symbols: identifiers, char literals like "'('"
    prec: Optional[str]
    action: Optional[str]  # C++ action body without the outer braces
    line: int


@dataclass
class Rule:
    lhs: str
    alternatives: List[Alternative]


@dataclass
class Grammar:
    token_types: Dict[str, Optional[str]] = field(default_factory=dict)  # token -> tag
    token_order: List[str] = field(default_factory=list)
    nonterminal_types: Dict[str, str] = field(default_factory=dict)  # nonterminal -> tag
    stripped_declarations: List[str] = field(default_factory=list)
    rules: List[Rule] = field(default_factory=list)


def strip_c_comments(text: str) -> str:
    """Remove /* */ and // comments. String and char literals are kept."""
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c in ('"', "'"):
            j = i + 1
            while j < n and text[j] != c:
                if text[j] == '\\':
                    j += 1
                j += 1
            out.append(text[i:j + 1])
            i = j + 1
        elif text.startswith('/*', i):
            j = text.find('*/', i + 2)
            if j < 0:
                j = n
            # keep newlines so that line numbers stay meaningful
            out.append('\n' * text[i:j].count('\n'))
            i = j + 2
        elif text.startswith('//', i):
            j = text.find('\n', i)
            if j < 0:
                j = n
            i = j
        else:
            out.append(c)
            i += 1
    return ''.join(out)


def split_sections(text: str) -> Tuple[str, str, str]:
    parts = re.split(r'^%%[ \t]*$', text, flags=re.M)
    if len(parts) < 2:
        sys.exit('grammar file has no %% separator')
    epilogue = parts[2] if len(parts) > 2 else ''
    return parts[0], parts[1], epilogue


def parse_declarations(decl: str, grammar: Grammar) -> None:
    # Remove %{ ... %} code blocks.
    decl = re.sub(r'%\{.*?%\}', '', decl, flags=re.S)
    # Remove the %union block.
    decl = re.sub(r'%union\s*\{.*?\}', '', decl, flags=re.S)
    decl = strip_c_comments(decl)
    # Split into directive chunks. A chunk starts at a line that begins with %.
    chunks = re.split(r'^(?=%)', decl, flags=re.M)
    for chunk in chunks:
        chunk = chunk.strip()
        if not chunk:
            continue
        m = re.match(r'%([A-Za-z_-]+)', chunk)
        if not m:
            sys.exit(f'unexpected declaration text: {chunk[:60]!r}')
        directive = m.group(1)
        rest = chunk[m.end():]
        if directive in ('define', 'name-prefix', 'locations', 'parse-param',
                         'lex-param', 'pure-parser', 'pure_parser', 'glr-parser',
                         'destructor', 'printer', 'initial-action', 'code',
                         'verbose', 'error-verbose', 'debug'):
            continue
        if directive == 'token':
            tag = None
            tm = re.match(r'\s*<(\w+)>', rest)
            if tm:
                tag = tm.group(1)
                rest = rest[tm.end():]
            names = parse_symbol_list(rest)
            for name in names:
                if name not in grammar.token_types:
                    grammar.token_order.append(name)
                grammar.token_types[name] = tag
            grammar.stripped_declarations.append('%token ' + ' '.join(names))
        elif directive == 'type':
            tm = re.match(r'\s*<(\w+)>', rest)
            if not tm:
                sys.exit(f'%type without tag: {chunk[:60]!r}')
            tag = tm.group(1)
            for name in parse_symbol_list(rest[tm.end():]):
                grammar.nonterminal_types[name] = tag
        elif directive in ('left', 'right', 'nonassoc', 'precedence'):
            names = parse_symbol_list(rest)
            for name in names:
                if name not in grammar.token_types and not name.startswith("'"):
                    grammar.token_order.append(name)
                    grammar.token_types[name] = None
            grammar.stripped_declarations.append(f'%{directive} ' + ' '.join(names))
        elif directive in ('expect', 'expect-rr', 'start'):
            grammar.stripped_declarations.append(f'%{directive} ' + rest.strip())
        else:
            sys.exit(f'unsupported declaration directive %{directive}')


def parse_symbol_list(text: str) -> List[str]:
    """Split a whitespace separated symbol list. Char literals stay quoted."""
    names = []
    for tok in re.findall(r"'(?:\\.|[^'\\])'|\"(?:\\.|[^\"\\])*\"|[A-Za-z_][A-Za-z0-9_.]*|<\w+>", text):
        if tok.startswith('<'):
            sys.exit(f'unexpected tag in the middle of a symbol list: {text[:60]!r}')
        names.append(tok)
    return names


def tokenize_rules(text: str) -> List[Tuple[str, str, int]]:
    """Tokenize the rules section into (kind, text, line) tuples.

    Kinds: ID, CHAR, STRING, COLON, PIPE, SEMI, PREC, EMPTY, ACTION.
    """
    tokens = []
    i = 0
    n = len(text)
    line = 1
    while i < n:
        c = text[i]
        if c == '\n':
            line += 1
            i += 1
        elif c in ' \t\r\f':
            i += 1
        elif text.startswith('/*', i):
            j = text.find('*/', i + 2)
            if j < 0:
                sys.exit('unterminated comment in rules section')
            line += text[i:j].count('\n')
            i = j + 2
        elif text.startswith('//', i):
            j = text.find('\n', i)
            i = n if j < 0 else j
        elif c == '{':
            depth = 0
            j = i
            start_line = line
            while j < n:
                ch = text[j]
                if ch == '\n':
                    line += 1
                    j += 1
                elif ch in ('"', "'"):
                    k = j + 1
                    while k < n and text[k] != ch:
                        if text[k] == '\\':
                            k += 1
                        if text[k] == '\n':
                            line += 1
                        k += 1
                    j = k + 1
                elif text.startswith('/*', j):
                    k = text.find('*/', j + 2)
                    line += text[j:k].count('\n')
                    j = k + 2
                elif text.startswith('//', j):
                    k = text.find('\n', j)
                    j = n if k < 0 else k
                elif ch == '{':
                    depth += 1
                    j += 1
                elif ch == '}':
                    depth -= 1
                    j += 1
                    if depth == 0:
                        break
                else:
                    j += 1
            tokens.append(('ACTION', text[i + 1:j - 1], start_line))
            i = j
        elif c == "'":
            j = i + 1
            while text[j] != "'":
                if text[j] == '\\':
                    j += 1
                j += 1
            tokens.append(('CHAR', text[i:j + 1], line))
            i = j + 1
        elif c == '"':
            j = i + 1
            while text[j] != '"':
                if text[j] == '\\':
                    j += 1
                j += 1
            tokens.append(('STRING', text[i:j + 1], line))
            i = j + 1
        elif c == ':':
            tokens.append(('COLON', ':', line))
            i += 1
        elif c == '|':
            tokens.append(('PIPE', '|', line))
            i += 1
        elif c == ';':
            tokens.append(('SEMI', ';', line))
            i += 1
        elif c == '%':
            m = re.match(r'%(\w+)', text[i:])
            word = m.group(1)
            if word == 'prec':
                tokens.append(('PREC', '%prec', line))
            elif word == 'empty':
                tokens.append(('EMPTY', '%empty', line))
            else:
                sys.exit(f'unsupported directive in rules section: %{word} (line {line})')
            i += m.end()
        else:
            m = re.match(r'[A-Za-z_][A-Za-z0-9_.]*', text[i:])
            if not m:
                sys.exit(f'unexpected character {c!r} in rules section at line {line}')
            tokens.append(('ID', m.group(0), line))
            i += m.end()
    return tokens


def parse_rules(text: str, grammar: Grammar) -> None:
    toks = tokenize_rules(text)
    i = 0
    n = len(toks)
    while i < n:
        kind, value, line = toks[i]
        if kind != 'ID' or i + 1 >= n or toks[i + 1][0] != 'COLON':
            sys.exit(f'expected "nonterminal :" at line {line}, got {kind} {value!r}')
        lhs = value
        i += 2
        rule = Rule(lhs, [])
        symbols: List[str] = []
        prec: Optional[str] = None
        action: Optional[str] = None
        alt_line = line

        def finish_alternative():
            rule.alternatives.append(Alternative(list(symbols), prec, action, alt_line))

        while i < n:
            kind, value, line = toks[i]
            if kind == 'SEMI':
                finish_alternative()
                i += 1
                break
            if kind == 'PIPE':
                finish_alternative()
                symbols, prec, action = [], None, None
                alt_line = line
                i += 1
                continue
            if kind in ('ID', 'CHAR', 'STRING'):
                if action is not None:
                    sys.exit(f'mid-rule action is not supported (line {line}, rule {lhs})')
                symbols.append(value)
                i += 1
            elif kind == 'PREC':
                prec = toks[i + 1][1]
                i += 2
            elif kind == 'EMPTY':
                i += 1
            elif kind == 'ACTION':
                if action is not None:
                    sys.exit(f'two actions in one alternative (line {line}, rule {lhs})')
                action = value
                i += 1
            else:
                sys.exit(f'unexpected token {kind} at line {line}')
        else:
            sys.exit(f'rule {lhs} is not terminated by ";"')
        grammar.rules.append(rule)


def parse_grammar(path: str) -> Grammar:
    text = open(path, encoding='utf-8', errors='surrogateescape').read()
    decl, rules, _epilogue = split_sections(text)
    grammar = Grammar()
    parse_declarations(decl, grammar)
    parse_rules(rules, grammar)
    return grammar


# ---------------------------------------------------------------------------
# Bison
# ---------------------------------------------------------------------------


def write_stripped_grammar(grammar: Grammar, path: str) -> None:
    lines = list(grammar.stripped_declarations)
    lines.append('%%')
    for rule in grammar.rules:
        alts = []
        for alt in rule.alternatives:
            body = ' '.join(alt.symbols)
            if alt.prec:
                body += ' %prec ' + alt.prec
            alts.append(body + ' {}')
        lines.append(f'{rule.lhs}:\n    ' + '\n  | '.join(alts) + '\n  ;')
    lines.append('%%')
    with open(path, 'w') as f:
        f.write('\n'.join(lines) + '\n')


@dataclass
class BisonOutput:
    tables: Dict[str, List[int]]
    constants: Dict[str, int]
    token_numbers: Dict[str, int]  # token name -> external token number
    symbol_names: List[str]  # yytname
    rule_offset: int  # bison rule number of the first user rule
    rule_lhs_symbols: List[int]  # by bison rule number


def run_bison(stripped_path: str, workdir: str) -> BisonOutput:
    tab_c = os.path.join(workdir, 'grammar.tab.c')
    tab_h = os.path.join(workdir, 'grammar.tab.h')
    xml_path = os.path.join(workdir, 'grammar.xml')
    cmd = ['bison', '-o', tab_c, f'--defines={tab_h}', f'--xml={xml_path}', stripped_path]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f'bison failed:\n{proc.stdout}\n{proc.stderr}')
    if 'conflict' in proc.stderr:
        sys.exit(f'bison reports conflicts:\n{proc.stderr}')
    src = open(tab_c).read()
    tables = {}
    table_types = {}
    for m in re.finditer(r'static const (\w+) (yy\w+)\[\] =\s*\{(.*?)\};', src, flags=re.S):
        ctype = m.group(1)
        name = m.group(2)
        body = m.group(3)
        if name == 'yytname':
            continue
        nums = [int(x) for x in re.findall(r'-?\d+', body)]
        tables[name] = nums
        table_types[name] = {'yytype_int8': 'i8', 'yytype_uint8': 'u8', 'yytype_int16': 'i16',
                             'yytype_uint16': 'u16', 'int': 'i32'}.get(ctype, 'i32')
    tables['__types__'] = table_types
    constants = {}
    for m in re.finditer(r'#define (YY[A-Z_]+)\s+\(?(-?\d+)\)?', src):
        constants[m.group(1)] = int(m.group(2))
    # Symbol names: prefer the XML, which has all symbols in order.
    tree = ET.parse(xml_path)
    root = tree.getroot()
    grammar_el = root.find('grammar')
    symbols = {}
    for t in grammar_el.find('terminals'):
        symbols[int(t.get('symbol-number'))] = t.get('name')
    for t in grammar_el.find('nonterminals'):
        symbols[int(t.get('symbol-number'))] = t.get('name')
    nsymbols = max(symbols) + 1
    symbol_names = [symbols.get(i, f'$symbol{i}') for i in range(nsymbols)]
    if 1 in symbols and symbols[1] != 'error':
        sys.exit('unexpected symbol numbering')
    symbol_names[1] = 'error'
    symbol_names[2] = '$undefined'
    # Token numbers from the header.
    hsrc = open(tab_h).read()
    enum = re.search(r'enum yytokentype\s*\{(.*?)\}', hsrc, flags=re.S)
    token_numbers = {}
    for m in re.finditer(r'(\w+)\s*=\s*(-?\d+)', enum.group(1)):
        token_numbers[m.group(1)] = int(m.group(2))
    # Rules from the XML. The XML numbers rules from 0 ($accept). The tables
    # (yyr1, yyr2, yydefact) use their own numbering. Find the shift between
    # the two by matching yyr1 against the left-hand side symbols.
    rule_lhs_symbols = []
    name_to_symbol = {v: k for k, v in symbols.items()}
    name_to_symbol['$accept'] = name_to_symbol.get('$accept', constants['YYNTOKENS'])
    for r in grammar_el.find('rules'):
        lhs = r.find('lhs').text
        rule_lhs_symbols.append(name_to_symbol[lhs])
    yyr1 = tables['yyr1']
    shift = None
    for candidate in (0, 1, 2):
        if all(yyr1[candidate + i] == sym for i, sym in enumerate(rule_lhs_symbols)):
            shift = candidate
            break
    if shift is None:
        sys.exit('cannot align the XML rule numbers with yyr1')
    # The first user rule is XML rule 1 ($accept is XML rule 0).
    rule_offset = shift + 1
    return BisonOutput(tables, constants, token_numbers, symbol_names, rule_offset, rule_lhs_symbols)


def check_rule_order(grammar: Grammar, bison: BisonOutput, workdir: str) -> None:
    """Make sure that bison numbered the rules in the order we parsed them."""
    tree = ET.parse(os.path.join(workdir, 'grammar.xml'))
    rules_el = tree.getroot().find('grammar').find('rules')
    bison_rules = []
    for r in rules_el:
        lhs = r.find('lhs').text
        if lhs == '$accept':
            continue
        rhs = [s.text for s in r.find('rhs') if s.tag == 'symbol']
        bison_rules.append((lhs, rhs))
    ours = []
    for rule in grammar.rules:
        for alt in rule.alternatives:
            ours.append((rule.lhs, [s for s in alt.symbols]))
    if len(ours) != len(bison_rules):
        sys.exit(f'rule count mismatch: parsed {len(ours)}, bison {len(bison_rules)}')
    for idx, (a, b) in enumerate(zip(ours, bison_rules)):
        if a[0] != b[0] or a[1] != b[1]:
            sys.exit(f'rule {idx} mismatch: parsed {a}, bison {b}')


# ---------------------------------------------------------------------------
# Action translation
# ---------------------------------------------------------------------------


def rust_string(s: str) -> str:
    """Return a Rust string literal for the body of a C string literal."""
    # The C literal escapes in the grammar are limited to \" and \\.
    body = s
    body = body.replace('\\\\', '\x00')
    body = body.replace('\\"', '"')
    body = body.replace('\x00', '\\')
    body = body.replace('\\', '\\\\').replace('"', '\\"')
    return '"' + body + '"'


def ir_type_variant(name: str) -> str:
    assert name.startswith('k'), name
    return name[1:]


def data_type_variant(name: str) -> str:
    assert name.startswith('kData'), name
    return name[5:]


DATA_FLAG_VARIANTS = {
    'kUse': 'Use', 'kMapToClosestOne': 'MapToClosestOne', 'kNoSplit': 'NoSplit',
    'kGlobal': 'Global', 'kReplace': 'Replace', 'kUndefine': 'Undefine',
    'kAlias': 'Alias', 'kMapToAll': 'MapToAll', 'kDefine': 'Define',
    'kNoModi': 'NoModi', 'kFlagUnknown': 'Unknown',
}

DATA_FLAG_VALUES = {
    'kUse': 0x8, 'kMapToClosestOne': 0x10, 'kNoSplit': 0x100, 'kGlobal': 0x4,
    'kReplace': 0x40, 'kUndefine': 0x2, 'kAlias': 0x80, 'kMapToAll': 0x20,
    'kDefine': 0x1, 'kNoModi': 0x200, 'kFlagUnknown': 0x0,
}


class ActionTranslator:
    def __init__(self, grammar: Grammar):
        self.grammar = grammar
        self.unhandled: List[Tuple[str, int, str]] = []

    def symbol_type(self, symbol: str) -> Optional[str]:
        if symbol in self.grammar.nonterminal_types:
            return self.grammar.nonterminal_types[symbol]
        if symbol in self.grammar.token_types:
            return self.grammar.token_types[symbol]
        return None

    # -- expressions -------------------------------------------------------

    def value_ref(self, n: int, symbols: List[str], want: str, rule: str) -> str:
        """Rust expression for $n with the given expected kind."""
        if n < 1 or n > len(symbols):
            sys.exit(f'rule {rule}: ${n} is out of range')
        sym_type = self.symbol_type(symbols[n - 1])
        idx = n - 1
        if want == 'ir':
            if sym_type != 'ir':
                sys.exit(f'rule {rule}: ${n} ({symbols[n-1]}) used as IR but has type {sym_type}')
            return f'rhs[{idx}].ir()'
        if want == 'str':
            if sym_type != 'str':
                sys.exit(f'rule {rule}: ${n} ({symbols[n-1]}) used as string but has type {sym_type}')
            return f'rhs[{idx}].str()'
        if want == 'sref':
            if sym_type != 'str':
                sys.exit(f'rule {rule}: ${n} ({symbols[n-1]}) used as string but has type {sym_type}')
            return f'rhs[{idx}].sref()'
        if want == 'ival':
            if sym_type != 'ival':
                sys.exit(f'rule {rule}: ${n} ({symbols[n-1]}) used as int but has type {sym_type}')
            return f'rhs[{idx}].ival()'
        if want == 'any':
            if sym_type == 'ir':
                return f'rhs[{idx}].ir()'
            if sym_type == 'str':
                return f'rhs[{idx}].str()'
            if sym_type == 'ival':
                return f'rhs[{idx}].ival()'
            sys.exit(f'rule {rule}: ${n} ({symbols[n-1]}) has no type')
        raise AssertionError(want)

    def translate_op(self, text: str) -> str:
        text = text.strip()
        if text == 'OP0()':
            return 'Op::empty()'
        m = re.match(r'OP3\(\s*("(?:[^"\\]|\\.)*")\s*,\s*("(?:[^"\\]|\\.)*")\s*,\s*("(?:[^"\\]|\\.)*")\s*\)$', text)
        if m:
            return 'Op::new({}, {}, {})'.format(*[rust_string(g[1:-1]) for g in m.groups()])
        m = re.match(r'OPMID\(\s*("(?:[^"\\]|\\.)*")\s*\)$', text)
        if m:
            return f'Op::mid({rust_string(m.group(1)[1:-1])})'
        raise ValueError(f'unsupported operator constructor: {text}')

    def translate_new_ir(self, text: str, symbols: List[str], rule: str) -> str:
        """Translate `new IR(...)` to a Rust IrTree call."""
        m = re.match(r'new IR\((.*)\)$', text.strip(), flags=re.S)
        if not m:
            raise ValueError(f'not a constructor: {text}')
        args = split_args(m.group(1))
        ty = 'IrType::' + ir_type_variant(args[0].strip())
        if len(args) >= 2 and re.match(r'OP[03]|OPMID', args[1].strip()):
            op = self.translate_op(args[1])
            children = [a.strip() for a in args[2:]]
            if len(children) > 2:
                raise ValueError(f'too many children: {text}')
            rendered = []
            for ch in children:
                rendered.append(f'Some({self.translate_child(ch, symbols, rule)})')
            while len(rendered) < 2:
                rendered.append('None')
            return f't.node({ty}, {op}, {rendered[0]}, {rendered[1]})'
        # Leaf constructors.
        value = args[1].strip()
        if value.startswith('string('):
            inner = value[len('string('):-1].strip()
            if inner.startswith('$'):
                val = self.value_ref(int(inner[1:]), symbols, 'str', rule)
            elif inner.startswith('"'):
                val = rust_string(inner[1:-1]) + '.to_string()'
            else:
                raise ValueError(f'unsupported string() argument: {text}')
            kind = 'str'
        elif value.startswith('to_string('):
            inner = value[len('to_string('):-1].strip()
            val = self.value_ref(int(inner[1:]), symbols, 'ival', rule) + '.to_string()'
            kind = 'str'
        elif value.startswith('$'):
            val = self.value_ref(int(value[1:]), symbols, 'any', rule)
            kind = 'any'
        else:
            raise ValueError(f'unsupported leaf constructor: {text}')
        rest = [a.strip() for a in args[2:]]
        if not rest:
            data_type, scope, flag = 'DataType::Whatever', '-1', 'DataFlag::Use'
        elif len(rest) == 3:
            data_type = 'DataType::' + data_type_variant(rest[0])
            scope = rest[1]
            flag = 'DataFlag::' + DATA_FLAG_VARIANTS[rest[2]]
        elif len(rest) == 2:
            # C++ overload quirk: IR(type, str, data_type, int scope, flag = kUse).
            # A flag passed as the fourth argument becomes the scope.
            data_type = 'DataType::' + data_type_variant(rest[0])
            scope = str(DATA_FLAG_VALUES[rest[1]])
            flag = 'DataFlag::Use'
        else:
            raise ValueError(f'unsupported constructor arguments: {text}')
        if kind == 'str':
            return f't.leaf_str({ty}, {val}, {data_type}, {scope}, {flag})'
        return f't.leaf_value({ty}, {val}, {data_type}, {scope}, {flag})'

    def translate_child(self, text: str, symbols: List[str], rule: str) -> str:
        text = text.strip()
        if text.startswith('$'):
            return self.value_ref(int(text[1:]), symbols, 'ir', rule)
        if re.match(r'^[A-Za-z_]\w*$', text):
            return text
        raise ValueError(f'unsupported child expression: {text}')

    def translate_condition(self, cond: str, symbols: List[str], rule: str) -> str:
        c = cond
        # strcmp forms
        c = re.sub(r'!\s*strcmp\(\s*\$(\d+)\s*,\s*("(?:[^"\\]|\\.)*")\s*\)',
                   lambda m: f'{self.value_ref(int(m.group(1)), symbols, "sref", rule)} == {rust_string(m.group(2)[1:-1])}', c)
        c = re.sub(r'strcmp\(\s*\$(\d+)\s*,\s*("(?:[^"\\]|\\.)*")\s*\)\s*==\s*0',
                   lambda m: f'{self.value_ref(int(m.group(1)), symbols, "sref", rule)} == {rust_string(m.group(2)[1:-1])}', c)
        c = re.sub(r'strcmp\(\s*\$(\d+)\s*,\s*("(?:[^"\\]|\\.)*")\s*\)\s*!=\s*0',
                   lambda m: f'{self.value_ref(int(m.group(1)), symbols, "sref", rule)} != {rust_string(m.group(2)[1:-1])}', c)
        c = re.sub(r'!\s*strcmp\(\s*(\w+)->get_prefix\(\)\s*,\s*("(?:[^"\\]|\\.)*")\s*\)',
                   lambda m: f't.prefix({m.group(1)}) == {rust_string(m.group(2)[1:-1])}', c)
        c = re.sub(r'!\s*strcmp\(\s*\$(\d+)->get_prefix\(\)\s*,\s*("(?:[^"\\]|\\.)*")\s*\)',
                   lambda m: f't.prefix({self.value_ref(int(m.group(1)), symbols, "ir", rule)}) == {rust_string(m.group(2)[1:-1])}', c)
        c = re.sub(r'!\s*\$(\d+)->is_empty\(\)',
                   lambda m: f'!t.is_empty({self.value_ref(int(m.group(1)), symbols, "ir", rule)})', c)
        c = re.sub(r'\$(\d+)->is_empty\(\)',
                   lambda m: f't.is_empty({self.value_ref(int(m.group(1)), symbols, "ir", rule)})', c)
        c = re.sub(r'!\s*(\w+)->is_empty\(\)', lambda m: f'!t.is_empty({m.group(1)})', c)
        c = re.sub(r'(\w+)->is_empty\(\)', lambda m: f't.is_empty({m.group(1)})', c)
        # Null checks on IR pointers or strings are always true in Rust.
        c = re.sub(r'(?<![\w$])\$(\d+)(?![\w\->])', 'true', c)
        c = re.sub(r'^\s*(tmp\d+)\s*$', 'true', c)
        return c.strip()

    # -- statements --------------------------------------------------------

    def translate_action(self, action: str, lhs: str, symbols: List[str], line: int) -> str:
        rule = f'{lhs} (line {line})'
        lhs_type = self.symbol_type(lhs)
        text = strip_c_comments(action)
        statements = join_statements(text)
        out: List[str] = []
        uses_res = re.search(r'\bres\b', text) is not None
        if uses_res:
            out.append('let mut res: IrId;')
        for stmt in statements:
            try:
                rendered = self.translate_statement(stmt, lhs_type, symbols, rule)
            except ValueError as e:
                self.unhandled.append((lhs, line, f'{stmt}  -- {e}'))
                rendered = f'compile_error!({rust_string("unhandled: " + stmt)});'
            if rendered:
                out.append(rendered)
        return '\n'.join(out)

    def translate_statement(self, stmt: str, lhs_type: Optional[str], symbols: List[str], rule: str) -> str:
        s = stmt.strip()
        if not s:
            return ''
        # Dropped statements: memory bookkeeping and debug output.
        if re.match(r'^(all_gen_ir|rov_ir)\.push_back\(.*\);$', s):
            return ''
        if re.match(r'^free\(.*\);$', s):
            return ''
        if re.match(r'^printf\(.*\);$', s):
            return ''
        if re.match(r'^res = \*pIR;$', s):
            return ''
        if re.match(r'^strcpy\(mem, ""\);$', s):
            return ''
        if s in ('{', '}'):
            return s
        if re.match(r'^\} else \{$', s):
            return '} else {'
        m = re.match(r'^(\}\s*)?else if \((.*)\) \{$', s, flags=re.S)
        if m:
            return f'}} else if {self.translate_condition(m.group(2), symbols, rule)} {{'
        m = re.match(r'^(\}\s*)?else\s*\{$', s)
        if m:
            return '} else {'
        m = re.match(r'^if \((.*)\)\s*\{$', s, flags=re.S)
        if m:
            return f'if {self.translate_condition(m.group(1), symbols, rule)} {{'
        m = re.match(r'^switch \((\w+)->(get_\w+)\(\)\) \{$', s)
        if m:
            return f'match t.{m.group(2)}({m.group(1)}) {{'
        m = re.match(r'^case (\d+): (.*); break;$', s)
        if m:
            return f'{m.group(1)} => {{ {self.translate_statement(m.group(2) + ";", lhs_type, symbols, rule)} }}'
        if re.match(r'^default: break;$', s):
            return '_ => {}'
        # Declarations with no value.
        m = re.match(r'^IR\* (\w+);$', s)
        if m:
            return f'let mut {m.group(1)}: IrId;'
        # Variable assignments from $N.
        m = re.match(r'^(?:auto|IR\*) (\w+) = \$(\d+);$', s)
        if m:
            return f'let mut {m.group(1)} = {self.value_ref(int(m.group(2)), symbols, "any", rule)};'
        # Constructors.
        m = re.match(r'^(?:auto |IR\* )?(\w+) = (new IR\(.*\));$', s, flags=re.S)
        if m:
            target = m.group(1)
            ctor = self.translate_new_ir(m.group(2), symbols, rule)
            if s.startswith('auto ') or s.startswith('IR* '):
                if target == 'res':
                    return f'res = {ctor};'
                return f'let mut {target} = {ctor};'
            return f'{target} = {ctor};'
        m = re.match(r'^\*pIR = (new IR\(.*\));$', s, flags=re.S)
        if m:
            ctor = self.translate_new_ir(m.group(1), symbols, rule)
            return f'res = {ctor};\nt.root = Some(res);'
        m = re.match(r'^\$\$ = (new IR\(.*\));$', s, flags=re.S)
        if m:
            ctor = self.translate_new_ir(m.group(1), symbols, rule)
            return f'yyval = YyVal::Ir({ctor});'
        # Type setters.
        m = re.match(r'^(\w+)\s*->\s*(set_\w+)\((kData\w+), (k\w+)\);$', s)
        if m:
            return (f'let _ = t.{m.group(2)}({m.group(1)}, DataType::{data_type_variant(m.group(3))}, '
                    f'DataFlag::{DATA_FLAG_VARIANTS[m.group(4)]});')
        m = re.match(r'^(\w+)\s*->\s*(set_\w+)\(RelOptionType::(\w+)\);$', s)
        if m:
            return f'let _ = t.{m.group(2)}({m.group(1)}, RelOptionType::{m.group(3)});'
        # Result assignment.
        m = re.match(r'^\$\$ = (.*);$', s, flags=re.S)
        if m:
            return self.translate_result(m.group(1).strip(), lhs_type, symbols, rule)
        # String helpers.
        m = re.match(r'^char\* mem = alloc_and_cat\((.*)\);$', s)
        if m:
            return f'let mem = {self.translate_concat(m.group(1), symbols, rule)};'
        if re.match(r'^char\* mem = \(char\*\) calloc\(2, 1\);$', s):
            return 'let mem = String::new();'
        raise ValueError('no translation rule')

    def translate_result(self, expr: str, lhs_type: Optional[str], symbols: List[str], rule: str) -> str:
        if expr == 'res':
            return 'yyval = YyVal::Ir(res);'
        if expr == 'mem':
            return 'yyval = YyVal::Str(mem);'
        m = re.match(r'^strdup\(\$(\d+)\)$', expr)
        if m:
            return f'yyval = YyVal::Str({self.value_ref(int(m.group(1)), symbols, "str", rule)});'
        m = re.match(r'^strdup\(("(?:[^"\\]|\\.)*")\)$', expr)
        if m:
            return f'yyval = YyVal::Str({rust_string(m.group(1)[1:-1])}.to_string());'
        m = re.match(r'^\$(\d+)$', expr)
        if m:
            return f'yyval = rhs[{int(m.group(1)) - 1}].take();'
        m = re.match(r'^alloc_and_cat\((.*)\)$', expr)
        if m:
            return f'yyval = YyVal::Str({self.translate_concat(m.group(1), symbols, rule)});'
        if re.match(r'^\w+$', expr) and lhs_type == 'ir':
            return f'yyval = YyVal::Ir({expr});'
        raise ValueError(f'unsupported $$ assignment: {expr}')

    def translate_concat(self, args: str, symbols: List[str], rule: str) -> str:
        parts = []
        for a in split_args(args):
            a = a.strip()
            if a.startswith('$'):
                parts.append(self.value_ref(int(a[1:]), symbols, 'sref', rule))
            elif a.startswith('"'):
                parts.append(rust_string(a[1:-1]))
            else:
                raise ValueError(f'unsupported concat argument {a}')
        fmt = '"' + '{}' * len(parts) + '"'
        return f'format!({fmt}, {", ".join(parts)})'


def split_args(text: str) -> List[str]:
    """Split a C argument list on top-level commas."""
    args = []
    depth = 0
    cur = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while text[j] != '"':
                if text[j] == '\\':
                    j += 1
                j += 1
            cur.append(text[i:j + 1])
            i = j + 1
            continue
        if c in '([':
            depth += 1
        elif c in ')]':
            depth -= 1
        if c == ',' and depth == 0:
            args.append(''.join(cur))
            cur = []
        else:
            cur.append(c)
        i += 1
    if cur and ''.join(cur).strip():
        args.append(''.join(cur))
    return args


def join_statements(text: str) -> List[str]:
    """Split action code into statements and block delimiters.

    A statement ends at ';' or at '{' or '}' outside of string literals and
    parentheses. `case N: stmt; break;` is kept together.
    """
    statements = []
    cur = []
    depth = 0
    i = 0
    n = len(text)

    def flush():
        s = ''.join(cur).strip()
        if s:
            statements.append(s)
        cur.clear()

    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while text[j] != '"':
                if text[j] == '\\':
                    j += 1
                j += 1
            cur.append(text[i:j + 1])
            i = j + 1
            continue
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        if depth == 0 and c == ';':
            cur.append(c)
            s = ''.join(cur).strip()
            # keep "case N: stmt; break;" together
            if re.match(r'^case \d+:', s) and not s.endswith('break;'):
                i += 1
                continue
            flush()
        elif depth == 0 and c == '{':
            cur.append(c)
            flush()
        elif depth == 0 and c == '}':
            flush()
            cur.append(c)
            # "} else {" and "} else if (...) {" are glued to the brace
            rest = text[i + 1:]
            m = re.match(r'\s*else\s*(if\s*\(.*?\))?\s*\{', rest, flags=re.S)
            if m:
                cur.append(' ' + rest[:m.end()].strip())
                i += 1 + m.end()
                flush()
                continue
            flush()
        else:
            cur.append(c)
        i += 1
    flush()
    return statements


# ---------------------------------------------------------------------------
# Emitters
# ---------------------------------------------------------------------------

HEADER = '// Generated by grammar/tools/gen_parser.py. Do not edit by hand.\n'


def emit_ir_types(define_h: str, out_dir: str) -> None:
    src = open(define_h).read()
    types = re.findall(r'^\s*V\((k\w+)\)', re.search(r'#define ALLTYPE\(V\)(.*?)\n\n', src, flags=re.S).group(1), flags=re.M)
    data_types = re.findall(r'^\s*V\((Data\w+)\)', re.search(r'#define ALLDATATYPE\(V\)(.*?)\n\n', src, flags=re.S).group(1), flags=re.M)
    types = ['kconst_str', 'kconst_int', 'kconst_float'] + types
    lines = [HEADER, '#![allow(non_camel_case_types, clippy::upper_case_acronyms)]', '',
             '/// The IR node types. One per grammar rule of the SQLRight grammar.',
             '#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]',
             '#[repr(u16)]', 'pub enum IrType {']
    for t in types:
        lines.append(f'    {ir_type_variant(t)},')
    lines.append('}')
    lines.append('')
    lines.append('impl IrType {')
    lines.append(f'    pub const COUNT: usize = {len(types)};')
    lines.append('    /// The C++ name of the type, for example `kSelectStmt`.')
    lines.append('    pub fn as_str(self) -> &\'static str {')
    lines.append('        IR_TYPE_NAMES[self as usize]')
    lines.append('    }')
    lines.append('    pub fn from_str(name: &str) -> Option<IrType> {')
    lines.append('        IR_TYPE_NAMES.iter().position(|n| *n == name).map(|i| ALL_IR_TYPES[i])')
    lines.append('    }')
    lines.append('    pub fn all() -> &\'static [IrType] {')
    lines.append('        &ALL_IR_TYPES')
    lines.append('    }')
    lines.append('}')
    lines.append('')
    lines.append(f'static IR_TYPE_NAMES: [&str; {len(types)}] = [')
    for t in types:
        lines.append(f'    "{t}",')
    lines.append('];')
    lines.append('')
    lines.append(f'static ALL_IR_TYPES: [IrType; {len(types)}] = [')
    for t in types:
        lines.append(f'    IrType::{ir_type_variant(t)},')
    lines.append('];')
    lines.append('')
    lines.append('/// The semantic role of an identifier or literal node.')
    lines.append('#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]')
    lines.append('#[repr(u8)]')
    lines.append('pub enum DataType {')
    for d in data_types:
        lines.append(f'    {d[4:]},')
    lines.append('}')
    lines.append('')
    lines.append('impl DataType {')
    lines.append('    /// The C++ name without the `k` prefix, for example `DataTableName`.')
    lines.append('    pub fn as_str(self) -> &\'static str {')
    lines.append('        DATA_TYPE_NAMES[self as usize]')
    lines.append('    }')
    lines.append('    pub fn from_str(name: &str) -> Option<DataType> {')
    lines.append('        DATA_TYPE_NAMES.iter().position(|n| *n == name).map(|i| ALL_DATA_TYPES[i])')
    lines.append('    }')
    lines.append('}')
    lines.append('')
    lines.append(f'static DATA_TYPE_NAMES: [&str; {len(data_types)}] = [')
    for d in data_types:
        lines.append(f'    "{d}",')
    lines.append('];')
    lines.append('')
    lines.append(f'static ALL_DATA_TYPES: [DataType; {len(data_types)}] = [')
    for d in data_types:
        lines.append(f'    DataType::{d[4:]},')
    lines.append('];')
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, 'ir_type.rs'), 'w') as f:
        f.write('\n'.join(lines) + '\n')


def rust_array(name: str, values: List[int], ty: str) -> str:
    rows = []
    for i in range(0, len(values), 16):
        rows.append('    ' + ', '.join(str(v) for v in values[i:i + 16]) + ',')
    return f'pub static {name}: [{ty}; {len(values)}] = [\n' + '\n'.join(rows) + '\n];\n'


def emit_tables(bison: BisonOutput, out_dir: str) -> None:
    lines = [HEADER, '#![allow(clippy::all)]', '']
    for const in ('YYFINAL', 'YYLAST', 'YYNTOKENS', 'YYNNTS', 'YYNRULES', 'YYNSTATES',
                  'YYMAXUTOK', 'YYPACT_NINF', 'YYTABLE_NINF'):
        if const not in bison.constants:
            sys.exit(f'bison output has no {const}')
        lines.append(f'pub const {const}: i32 = {bison.constants[const]};')
    lines.append(f'pub const RULE_OFFSET: usize = {bison.rule_offset};')
    lines.append('')
    for name in ('yytranslate', 'yypact', 'yydefact', 'yypgoto', 'yydefgoto', 'yytable', 'yycheck', 'yyr1', 'yyr2'):
        if name not in bison.tables:
            sys.exit(f'bison output has no table {name}')
        lines.append(rust_array(name.upper(), bison.tables[name], bison.tables['__types__'][name]))
    lines.append(f'pub static YYTNAME: [&str; {len(bison.symbol_names)}] = [')
    for s in bison.symbol_names:
        lines.append(f'    {rust_string(s)},')
    lines.append('];')
    with open(os.path.join(out_dir, 'tables.rs'), 'w') as f:
        f.write('\n'.join(lines) + '\n')


def emit_tokens(grammar: Grammar, bison: BisonOutput, kwlist_h: str, out_dir: str) -> None:
    lines = [HEADER, '#![allow(non_upper_case_globals, dead_code)]', '',
             '//! Token numbers as assigned by bison, and the keyword table.', '']
    for name, number in bison.token_numbers.items():
        if name in ('YYEMPTY',):
            continue
        lines.append(f'pub const {name}: i32 = {number};')
    lines.append('')
    # Keyword table from kwlist.h.
    keywords = []
    for m in re.finditer(r'PG_KEYWORD\("([^"]+)",\s*(\w+),\s*(\w+),\s*(\w+)\)', open(kwlist_h).read()):
        name, token, category, label = m.groups()
        if token not in bison.token_numbers:
            sys.exit(f'keyword {name} uses token {token}, which the grammar does not declare')
        keywords.append((name, token, category, label))
    keywords.sort()
    lines.append('/// Keyword categories, as in PostgreSQL `kwlist.h`.')
    lines.append('#[derive(Debug, Clone, Copy, PartialEq, Eq)]')
    lines.append('pub enum KeywordCategory {')
    lines.append('    Unreserved,')
    lines.append('    ColName,')
    lines.append('    TypeFuncName,')
    lines.append('    Reserved,')
    lines.append('}')
    lines.append('')
    lines.append('/// Keywords sorted by name. Each entry: (name, token number, category, can be a bare label).')
    lines.append(f'pub static KEYWORDS: [(&str, i32, KeywordCategory, bool); {len(keywords)}] = [')
    cat_map = {'UNRESERVED_KEYWORD': 'Unreserved', 'COL_NAME_KEYWORD': 'ColName',
               'TYPE_FUNC_NAME_KEYWORD': 'TypeFuncName', 'RESERVED_KEYWORD': 'Reserved'}
    max_len = 0
    for name, token, category, label in keywords:
        max_len = max(max_len, len(name))
        lines.append(f'    ("{name}", {token}, KeywordCategory::{cat_map[category]}, {"true" if label == "BARE_LABEL" else "false"}),')
    lines.append('];')
    lines.append(f'pub const MAX_KEYWORD_LEN: usize = {max_len};')
    with open(os.path.join(out_dir, 'tokens.rs'), 'w') as f:
        f.write('\n'.join(lines) + '\n')


def emit_actions(grammar: Grammar, bison: BisonOutput, out_dir: str) -> ActionTranslator:
    translator = ActionTranslator(grammar)
    arms = []  # (bison rule number, comment, body)
    number = bison.rule_offset
    for rule in grammar.rules:
        for alt in rule.alternatives:
            comment = f'{rule.lhs}: {" ".join(alt.symbols) or "/* empty */"}'
            if alt.action is None:
                if alt.symbols:
                    body = 'yyval = rhs[0].take();'
                else:
                    body = ''
            else:
                body = translator.translate_action(alt.action, rule.lhs, alt.symbols, alt.line)
            arms.append((number, comment, body))
            number += 1
    chunk_size = 400
    lines = [HEADER, '#![allow(unused_mut, unused_variables, unused_assignments, clippy::all)]', '',
             'use sqlright_ir::{DataFlag, DataType, IrId, IrTree, IrType, Op, RelOptionType};',
             'use crate::yyval::YyVal;', '',
             '/// Run the semantic action of `rule` (bison rule number).',
             '/// `rhs` holds the values of the right-hand side symbols.',
             'pub(crate) fn reduce(rule: usize, rhs: &mut [YyVal], t: &mut IrTree) -> YyVal {',
             f'    match rule / {chunk_size} {{']
    nchunks = (arms[-1][0] // chunk_size) + 1
    for c in range(nchunks):
        lines.append(f'        {c} => reduce_{c}(rule, rhs, t),')
    lines.append('        _ => unreachable!("rule {} has no action", rule),')
    lines.append('    }')
    lines.append('}')
    lines.append('')
    for c in range(nchunks):
        lines.append(f'fn reduce_{c}(rule: usize, rhs: &mut [YyVal], t: &mut IrTree) -> YyVal {{')
        lines.append('    let mut yyval = YyVal::None;')
        lines.append('    match rule {')
        for number, comment, body in arms:
            if number // chunk_size != c:
                continue
            lines.append(f'        {number} => {{')
            lines.append(f'            // {comment}')
            for bl in body.split('\n'):
                if bl.strip():
                    lines.append('            ' + bl)
            lines.append('        }')
        lines.append('        _ => unreachable!("rule {} has no action", rule),')
        lines.append('    }')
        lines.append('    yyval')
        lines.append('}')
        lines.append('')
    with open(os.path.join(out_dir, 'actions.rs'), 'w') as f:
        f.write('\n'.join(lines) + '\n')
    return translator


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--grammar-dir', required=True, help='directory with bison_parser.y, define.h, kwlist.h')
    ap.add_argument('--parser-out', required=True, help='output directory for the parser crate')
    ap.add_argument('--ir-out', required=True, help='output directory for the IR crate')
    ap.add_argument('--keep-work', help='keep the bison work files in this directory')
    args = ap.parse_args()

    grammar_path = os.path.join(args.grammar_dir, 'bison_parser.y')
    grammar = parse_grammar(grammar_path)
    nalts = sum(len(r.alternatives) for r in grammar.rules)
    print(f'parsed {len(grammar.rules)} rules, {nalts} alternatives, {len(grammar.token_order)} tokens')

    workdir = args.keep_work or tempfile.mkdtemp(prefix='sqlright-bison-')
    os.makedirs(workdir, exist_ok=True)
    stripped = os.path.join(workdir, 'grammar.y')
    write_stripped_grammar(grammar, stripped)
    bison = run_bison(stripped, workdir)
    check_rule_order(grammar, bison, workdir)
    print(f'bison: {bison.constants["YYNSTATES"]} states, {bison.constants["YYNRULES"]} rules, '
          f'{bison.constants["YYNTOKENS"]} tokens, first user rule {bison.rule_offset}')

    os.makedirs(args.parser_out, exist_ok=True)
    emit_ir_types(os.path.join(args.grammar_dir, 'define.h'), args.ir_out)
    emit_tables(bison, args.parser_out)
    emit_tokens(grammar, bison, os.path.join(args.grammar_dir, 'kwlist.h'), args.parser_out)
    translator = emit_actions(grammar, bison, args.parser_out)
    if translator.unhandled:
        print(f'{len(translator.unhandled)} statements were not translated:')
        for lhs, line, stmt in translator.unhandled:
            print(f'  {lhs} (line {line}): {stmt}')
        sys.exit(1)
    print('done')


if __name__ == '__main__':
    main()
