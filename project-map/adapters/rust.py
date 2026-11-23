"""Rust adapter for project-map.

Reads source text directly: no compiler, no build, no network. It extracts
module purpose, public surface, resolved internal imports and named constants.
It stops at test modules and does not try to understand function bodies.
"""

import re
from pathlib import Path

USE_LINE = re.compile(r"^(?P<public>(?:pub(?:\([^)]*\))?)\s+)?use\s+(?P<body>.*)$")
ITEM_LINE = re.compile(
    r"^(?P<public>(?:pub(?:\([^)]*\))?)\s+)?"
    r"(?:(?:async|unsafe|extern|const)\s+)*"
    r"(?P<kind>fn|struct|enum|trait|type|const|static|mod)\s+"
    r"(?P<name>[A-Za-z_]\w*)"
)
CONST_LINE = re.compile(
    r"\bconst\s+(?P<name>[A-Z][A-Z0-9_]*)\s*:\s*(?P<type>.+?)\s*=\s*"
    r"(?P<value>.+?);\s*$",
    re.S,
)
PATH_REF = re.compile(r"\b(?:crate|super)::[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*")
RAW_OPEN = re.compile(r"r(#+)?")


def code_text(line, string_state=0):
    """Blank strings and comments so path matches come from code only.

    State is 0 outside a string, -1 inside a normal string, and hashes+1
    inside a raw string, so multi-line literals cannot leak false edges.
    """
    chars = list(line)
    index = 0
    while index < len(chars):
        char = chars[index]
        if string_state == -1:
            chars[index] = " "
            if char == "\\":
                if index + 1 < len(chars):
                    chars[index + 1] = " "
                index += 2
                continue
            if char == '"':
                string_state = 0
        elif string_state > 0:
            chars[index] = " "
            if char == '"' and line[index:index + string_state] == '"' + "#" * (string_state - 1):
                for position in range(index, index + string_state):
                    chars[position] = " "
                index += string_state
                string_state = 0
                continue
        elif char == "/" and chars[index:index + 2] == ["/", "/"]:
            for position in range(index, len(chars)):
                chars[position] = " "
            break
        elif char == '"':
            chars[index] = " "
            string_state = -1
        elif char == "r":
            raw = RAW_OPEN.match(line, index)
            if raw and raw.end() < len(chars) and chars[raw.end()] == '"':
                hashes = len(raw.group(1) or "")
                for position in range(index, raw.end() + 1):
                    chars[position] = " "
                index = raw.end() + 1
                string_state = hashes + 1
                continue
        elif char == "'" and index + 2 < len(chars) and chars[index + 2] == "'":
            index += 3
            continue
        index += 1
    return "".join(chars), string_state


def fragment(text, in_string=False):
    """Brace delta and string state for one fragment, ignoring comments."""
    delta = 0
    index = 0
    while index < len(text):
        char = text[index]
        if in_string:
            if char == "\\":
                index += 2
                continue
            if char == '"':
                in_string = False
        elif char == "/" and text[index:index + 2] == "//":
            break
        elif char == '"':
            in_string = True
        elif char == "'" and index + 2 < len(text) and text[index + 2] == "'":
            index += 3
            continue
        elif char == "{":
            delta += 1
        elif char == "}":
            delta -= 1
        index += 1
    return delta, in_string


def statement(lines, start, state):
    """Collect one statement at top level; return its text, next line, state."""
    parts = []
    index = start
    while index < len(lines):
        parts.append(lines[index].strip())
        delta, string_state = fragment(" ".join(parts), state)
        if not string_state and (parts[-1].endswith(";") or delta > 0):
            return " ".join(parts), index + 1, string_state
        index += 1
    return " ".join(parts), len(lines), state


def module_doc(lines):
    """Leading //! block, with blank //! lines kept between paragraphs."""
    result = []
    started = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("//!"):
            result.append(stripped[3:].strip())
            started = True
        elif started and not stripped:
            result.append("")
        elif started:
            break
    while result and not result[-1]:
        result.pop()
    return "\n".join(result)


def preceding_docs(lines, index):
    """Doc lines directly above an item, skipping attribute lines."""
    result = []
    cursor = index - 1
    while cursor >= 0:
        stripped = lines[cursor].strip()
        if stripped.startswith("///"):
            result.append(stripped[3:].strip())
        elif not stripped.startswith("#["):
            break
        cursor -= 1
    return "\n".join(reversed(result))


def resolve(body, module_id, known):
    """Resolve a use body to known module ids by its longest matching prefix."""
    if body.startswith("super::"):
        prefix = module_id.split("::")[:-1]
        rest = body[len("super::"):]
    elif body.startswith("crate::"):
        prefix = []
        rest = body[len("crate::"):]
    else:
        return []

    group = re.search(r"\{([^{}]*)\}", rest)
    stem = rest[:group.start()].rstrip(":") if group else rest
    names = [part.strip() for part in group.group(1).split(",")] if group else [""]
    head = prefix + ([stem] if stem else [])
    targets = set()
    for name in names:
        parts = head + ([name] if name and name != "*" else [])
        candidate = "::".join(part for part in parts if part)
        while candidate and candidate not in known and "::" in candidate:
            candidate = candidate.rsplit("::", 1)[0]
        if candidate in known:
            targets.add(candidate)
    return sorted(targets)


def module_id_for(path, root):
    """A file's module id: mod.rs names its directory, others their stem."""
    relative = path.relative_to(root)
    parts = list(relative.parts)
    if parts[-1] == "mod.rs":
        parts.pop()
    else:
        parts[-1] = parts[-1][:-3]
    return "::".join(parts)


def parse_file(path, source_root, display_root, known):
    """Extract one module's contract-level shape."""
    lines = path.read_text().splitlines()
    relative = str(path.relative_to(display_root))
    module_id = module_id_for(path, source_root)
    items = []
    facts = []
    imports = {}
    paths = {}
    declares = []
    exports = []
    depth = 0
    in_string = False
    in_tests = False
    test_start = None
    owner = None
    index = 0

    while index < len(lines):
        stripped = lines[index].strip()
        if not in_tests and re.match(r"^#\[cfg\(test\)\]|^mod tests\b", stripped):
            in_tests = True
            test_start = index
        if not in_tests and depth == 0 and stripped.startswith("impl "):
            match = re.match(r"^impl(?:<[^>]*>)?\s+(?:.*?\s+for\s+)?([A-Za-z_]\w*)", stripped)
            if match:
                owner = match.group(1)
        if not in_tests and (depth == 0 or (owner is not None and depth == 1)) and not in_string:
            use = USE_LINE.match(stripped)
            item = ITEM_LINE.match(stripped)
            if use:
                text, index, in_string = statement(lines, index, in_string)
                body = use.group("body")
                targets = resolve(body.rstrip(";").strip(), module_id, known)
                for target in targets:
                    imports.setdefault(target, []).append(text)
                if use.group("public"):
                    exports.append(text)
                delta, in_string = fragment(text, in_string)
                depth = max(0, depth + delta)
                continue
            if item:
                start = index
                text, index, in_string = statement(lines, index, in_string)
                name = item.group("name")
                if owner is not None and depth == 1:
                    name = f"{owner}::{name}"
                if item.group("kind") == "mod" and text.rstrip().endswith(";"):
                    child = name if module_id in ("main", "lib") else f"{module_id}::{name}"
                    if child in known:
                        declares.append(child)
                if item.group("public"):
                    items.append({
                        "kind": item.group("kind"),
                        "name": name,
                        "signature": text[:240],
                        "doc": preceding_docs(lines, start),
                        "line": start + 1,
                    })
                constant = CONST_LINE.search(text)
                if item.group("kind") == "const" and constant:
                    facts.append({
                        "id": f"{module_id}::{constant.group('name')}",
                        "value": constant.group("value"),
                        "source": f"{relative}:{start + 1}",
                        "excerpt": text[:200],
                    })
                delta, in_string = fragment(text, in_string)
                depth = max(0, depth + delta)
                if depth == 0:
                    owner = None
                continue
        delta, in_string = fragment(lines[index], in_string)
        depth = max(0, depth + delta)
        if depth == 0:
            owner = None
        if not in_tests:
            for name in re.findall(r'"(ONELOOP_[A-Z0-9_]+)"', lines[index]):
                facts.append({
                    "id": f"env::{name}",
                    "value": name,
                    "source": f"{relative}:{index + 1}",
                    "excerpt": stripped[:200],
                })
        index += 1

    # A `use` is one edge; a fully qualified path elsewhere is another. Both
    # are dependency directions, and neither is implied by the other.
    string_state = 0
    for line in lines[:test_start if test_start is not None else len(lines)]:
        code, string_state = code_text(line, string_state)
        if USE_LINE.match(code.strip()):
            continue
        for reference in PATH_REF.finditer(code):
            for target in resolve(reference.group(0), module_id, known):
                if target != module_id:
                    paths.setdefault(target, []).append(code.strip()[:200])

    return {
        "id": module_id,
        "path": relative,
        "doc": module_doc(lines),
        "items": items,
        "exports": sorted(exports),
        "declares": sorted(declares),
        "imports": {target: sorted(set(texts)) for target, texts in sorted(imports.items())},
        "paths": {target: sorted(set(texts)) for target, texts in sorted(paths.items())},
        "lines": len(lines),
    }, facts


def scan(root, config):
    """Scan configured roots; module ids are assigned before parsing imports."""
    sources = [
        (root / directory, path)
        for directory in config["roots"]
        for path in sorted((root / directory).rglob("*.rs"))
    ]
    known = {module_id_for(path, source_root) for source_root, path in sources}
    modules = []
    facts = []
    for source_root, path in sources:
        module, module_facts = parse_file(path, source_root, root, known)
        modules.append(module)
        facts.extend(module_facts)
    unique_facts = {fact["id"]: fact for fact in sorted(facts, key=lambda fact: fact["source"])}
    return {
        "modules": sorted(modules, key=lambda module: module["id"]),
        "facts": [unique_facts[key] for key in sorted(unique_facts)],
    }
