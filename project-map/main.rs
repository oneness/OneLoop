//! Build the project map and validate the citations in the mental model.
//!
//! The driver owns the graph, the layering, the output format and the check.
//! Parsing is language-specific, but lives beside the driver in the same Rust
//! file for now: the tool reads source text directly, with no compiler, build
//! or network. The page itself is authored; this tool never invents a sentence.
//!
//! `./project-map/generate` writes the artifact. `./project-map/generate --check`
//! validates `<!-- module/edge/fact -->` citations without writing anything.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const CONFIG: &str = "project-map/config.json";

struct Settings {
    roots: Vec<String>,
    entry_points: Vec<String>,
    artifact: String,
    doc: String,
}

struct Item {
    kind: String,
    name: String,
    signature: String,
    doc: String,
    line: usize,
}

struct Fact {
    id: String,
    value: String,
    source: String,
    excerpt: String,
}

struct Module {
    id: String,
    path: String,
    doc: String,
    items: Vec<Item>,
    exports: Vec<String>,
    declares: Vec<String>,
    imports: BTreeMap<String, BTreeSet<String>>,
    paths: BTreeMap<String, BTreeSet<String>>,
    lines: usize,
}

struct Map {
    modules: Vec<Module>,
    edges: Vec<Value>,
    layers: Vec<Vec<String>>,
    unresolved: Vec<String>,
    facts: Vec<Fact>,
    settings: Settings,
}

struct Args {
    stdout: bool,
    check: bool,
    doc: Option<String>,
    out: Option<String>,
}

fn print_help() {
    println!(
        "project-map: build the map, check citations\n\
         \n\
         ./project-map/generate                  write the artifact\n\
         ./project-map/generate --stdout         write the artifact to stdout instead\n\
         ./project-map/generate --check          validate citations in the doc, exit 1 on a bad one\n\
         ./project-map/generate --doc PATH       check a different document\n\
         ./project-map/generate --out PATH       write the artifact somewhere else"
    );
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        stdout: false,
        check: false,
        doc: None,
        out: None,
    };
    let mut rest = std::env::args().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--stdout" => args.stdout = true,
            "--check" => args.check = true,
            "--doc" => args.doc = Some(rest.next().context("--doc needs a path")?),
            "--out" => args.out = Some(rest.next().context("--out needs a path")?),
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
    }
    Ok(args)
}

fn read_settings() -> Result<Settings> {
    let path = Path::new(ROOT).join(CONFIG);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let strings = |key: &str| -> Result<Vec<String>> {
        value
            .get(key)
            .and_then(Value::as_array)
            .with_context(|| format!("{CONFIG}: '{key}' must be an array of strings"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{CONFIG}: '{key}' must be an array of strings"))
            })
            .collect()
    };
    let string = |key: &str| -> Result<String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("{CONFIG}: '{key}' must be a string"))
    };
    Ok(Settings {
        roots: strings("roots")?,
        entry_points: strings("entry_points")?,
        artifact: string("artifact")?,
        doc: string("doc")?,
    })
}

fn rust_files(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in
            fs::read_dir(&current).with_context(|| format!("reading {}", current.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let relative = path
                    .strip_prefix(ROOT)
                    .with_context(|| format!("{} is outside the checkout", path.display()))?;
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `src/tools/edit.rs` -> `tools::edit`; `src/main.rs` -> `main`.
fn module_id(relative: &str, root: &str) -> String {
    let prefix = format!("{root}/");
    let stripped = relative.strip_prefix(&prefix).unwrap_or(relative);
    let mut parts: Vec<&str> = stripped.split('/').collect();
    let Some(leaf) = parts.pop() else {
        return stripped.to_owned();
    };
    if leaf != "mod.rs" {
        parts.push(leaf.strip_suffix(".rs").unwrap_or(leaf));
    }
    parts.join("::")
}

/// Blank string and comment contents out of a line, and count its braces.
/// State is 0 outside strings, -1 inside a normal string, hashes+1 inside a raw
/// string, so multi-line literals cannot leak false edges.
fn blank_and_delta(line: &str, state: &mut i32) -> (String, i32) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = vec![' '; chars.len()];
    let mut delta = 0;
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        if *state == -1 {
            if char == '\\' {
                if index + 1 < chars.len() {
                    out[index + 1] = ' ';
                }
                index += 2;
                continue;
            }
            if char == '"' {
                *state = 0;
            }
            index += 1;
            continue;
        }
        if *state > 0 {
            let hashes = (*state - 1) as usize;
            if char == '"' {
                let end = index + 1 + hashes;
                if end <= chars.len() && chars[index + 1..end].iter().all(|item| *item == '#') {
                    for slot in out.iter_mut().take(end).skip(index) {
                        *slot = ' ';
                    }
                    index = end;
                    *state = 0;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        if char == '/' && chars.get(index + 1) == Some(&'/') {
            break;
        }
        if char == '"' {
            *state = -1;
            index += 1;
            continue;
        }
        if char == 'r' {
            let mut end = index + 1;
            let mut hashes = 0;
            while chars.get(end) == Some(&'#') {
                hashes += 1;
                end += 1;
            }
            if chars.get(end) == Some(&'"') {
                *state = hashes + 1;
                index = end + 1;
                continue;
            }
        }
        if char == '\'' && chars.get(index + 2) == Some(&'\'') {
            index += 3;
            continue;
        }
        out[index] = char;
        if char == '{' {
            delta += 1;
        } else if char == '}' {
            delta -= 1;
        }
        index += 1;
    }
    (out.into_iter().collect(), delta)
}

/// Collect one statement, advancing past the lines it spans.
fn statement(
    lines: &[&str],
    start: usize,
    state: &mut i32,
    until_semicolon: bool,
) -> (String, usize, i32) {
    let mut parts = Vec::new();
    let mut index = start;
    let mut delta = 0;
    let mut local = *state;
    while index < lines.len() {
        let (code, change) = blank_and_delta(lines[index], &mut local);
        let trimmed = code.trim();
        parts.push(trimmed.to_owned());
        delta += change;
        index += 1;
        let done = if until_semicolon {
            trimmed.contains(';')
        } else {
            trimmed.ends_with(';') || delta > 0
        };
        if done || index >= lines.len() {
            break;
        }
    }
    *state = local;
    (parts.join(" "), index, delta)
}

fn module_doc(lines: &[&str]) -> String {
    let mut result = Vec::new();
    let mut started = false;
    for line in lines {
        let stripped = line.trim();
        if let Some(text) = stripped.strip_prefix("//!") {
            result.push(text.trim().to_owned());
            started = true;
        } else if started && stripped.is_empty() {
            result.push(String::new());
        } else if started {
            break;
        }
    }
    while result.last().is_some_and(String::is_empty) {
        result.pop();
    }
    result.join("\n")
}

fn preceding_docs(lines: &[&str], index: usize) -> String {
    let mut result = Vec::new();
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        let stripped = lines[cursor].trim();
        if let Some(text) = stripped.strip_prefix("///") {
            result.push(text.trim().to_owned());
        } else if !stripped.starts_with("#[") {
            break;
        }
    }
    result.reverse();
    result.join("\n")
}

/// An item declaration at the head of a line: visibility, kind and name.
fn item_head(line: &str) -> Option<(bool, String, String)> {
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let (public, mut word) = if first == "pub" || first.starts_with("pub(") {
        (true, words.next()?)
    } else {
        (false, first)
    };
    while ["async", "unsafe", "extern"].contains(&word) {
        word = words.next()?;
    }
    let kinds = [
        "fn", "struct", "enum", "trait", "type", "const", "static", "mod",
    ];
    if !kinds.contains(&word) {
        return None;
    }
    let name: String = words
        .next()?
        .chars()
        .take_while(|char| char.is_alphanumeric() || *char == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    Some((public, word.to_owned(), name))
}

/// Longest-known-prefix resolution of a `crate::` / `super::` path body.
fn resolve(body: &str, module: &str, known: &BTreeSet<String>) -> BTreeSet<String> {
    let body = body.replace(|char: char| char.is_whitespace(), "");
    let body = body.as_str();
    let (prefix, rest): (Vec<&str>, &str) = if let Some(rest) = body.strip_prefix("super::") {
        let mut parts: Vec<&str> = module.split("::").collect();
        parts.pop();
        (parts, rest)
    } else if let Some(rest) = body.strip_prefix("crate::") {
        (Vec::new(), rest)
    } else {
        return BTreeSet::new();
    };

    let (stem, names): (&str, Vec<&str>) = match rest.find('{') {
        Some(open) => {
            let close = rest[open..]
                .find('}')
                .map(|at| open + at)
                .unwrap_or(rest.len());
            let stem = rest[..open].trim_end_matches(':');
            let has_close = rest[open..].contains('}');
            let inner = rest.get(open + 1..close).unwrap_or("");
            let names = inner
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect();
            (if has_close { stem } else { rest }, names)
        }
        None => (rest, vec![""]),
    };

    let mut head: Vec<String> = prefix.iter().map(|part| (*part).to_owned()).collect();
    head.extend(
        stem.split("::")
            .filter(|part| !part.is_empty())
            .map(str::to_owned),
    );

    let mut targets = BTreeSet::new();
    for name in names {
        let mut candidate = head.clone();
        if !name.is_empty() && name != "*" && name != "self" {
            candidate.extend(name.split("::").map(str::to_owned));
        }
        let mut joined = candidate.join("::");
        while !joined.is_empty() && !known.contains(&joined) {
            match joined.rsplit_once("::") {
                Some((parent, _)) => joined = parent.to_owned(),
                None => break,
            }
        }
        if known.contains(&joined) {
            targets.insert(joined);
        }
    }
    targets
}

/// Fully qualified `crate::…` / `super::…` references in a line of code.
fn path_refs(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["crate::", "super::"] {
        let mut cursor = 0;
        while let Some(found) = code[cursor..].find(marker) {
            let start = cursor + found;
            let mut end = start + marker.len();
            while end < code.len() {
                let char = code[end..].chars().next().unwrap_or(' ');
                if char.is_ascii_alphanumeric() || char == '_' || char == ':' {
                    end += char.len_utf8();
                } else {
                    break;
                }
            }
            out.push(code[start..end].to_owned());
            cursor = end;
        }
    }
    out.sort();
    out
}

fn env_facts_in(line: &str, source: &str, number: usize) -> Vec<Fact> {
    if line.trim_start().starts_with("//") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for literal in line.split('"').skip(1).step_by(2) {
        if literal.starts_with("ONELOOP_")
            && literal
                .chars()
                .all(|char| char.is_ascii_uppercase() || char.is_ascii_digit() || char == '_')
        {
            out.push(Fact {
                id: format!("env::{literal}"),
                value: literal.to_owned(),
                source: format!("{source}:{number}"),
                excerpt: line.trim().chars().take(200).collect(),
            });
        }
    }
    out
}

fn const_fact(text: &str, module: &str, source: &str, line: usize) -> Option<Fact> {
    let after = text.split_once("const ")?.1;
    let (name, rest) = after.split_once(':')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|char| char.is_ascii_uppercase() || char.is_ascii_digit() || char == '_')
    {
        return None;
    }
    let value = rest.split_once('=')?.1.trim().trim_end_matches(';').trim();
    if value.is_empty() || value.ends_with('{') || (value.starts_with('&') && value.contains('{')) {
        // A struct or slice literal holds no stable scalar to assert.
        return None;
    }
    Some(Fact {
        id: format!("{module}::{name}"),
        value: value.to_owned(),
        source: format!("{source}:{line}"),
        excerpt: text.chars().take(200).collect(),
    })
}

fn owner_of(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("impl ")?;
    let type_part = rest.rsplit_once(" for ").map_or(rest, |(_, ty)| ty);
    let name: String = type_part
        .trim()
        .chars()
        .take_while(|char| char.is_alphanumeric() || *char == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn parse_file(path: &str, root: &str, known: &BTreeSet<String>) -> Result<(Module, Vec<Fact>)> {
    let text = fs::read_to_string(Path::new(ROOT).join(path))
        .with_context(|| format!("reading {path}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let module = module_id(path, root);
    let mut items = Vec::new();
    let mut facts = Vec::new();
    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut paths: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut exports = BTreeSet::new();
    let mut declares = BTreeSet::new();
    let mut depth = 0i32;
    let mut state = 0i32;
    let mut owner: Option<String> = None;
    let mut test_start = lines.len();
    let mut index = 0;

    while index < lines.len() {
        let stripped = lines[index].trim();
        if stripped.starts_with("#[cfg(test)]") || stripped.starts_with("mod tests ") {
            test_start = index;
            break;
        }
        if depth == 0 && stripped.starts_with("impl ") {
            owner = owner_of(stripped);
        }
        if (depth == 0 || (owner.is_some() && depth == 1)) && state == 0 {
            let is_use = stripped.starts_with("use ")
                || stripped.starts_with("pub use ")
                || stripped.starts_with("pub(crate) use ");
            if is_use {
                let start = index;
                let (statement_text, next, change) = statement(&lines, index, &mut state, true);
                let public = stripped.starts_with("pub ");
                let body = statement_text
                    .split_once("use ")
                    .map_or("", |(_, body)| body)
                    .trim_end_matches(';')
                    .trim();
                for target in resolve(body, &module, known) {
                    if target != module {
                        imports
                            .entry(target)
                            .or_default()
                            .insert(statement_text.clone());
                    }
                }
                if public {
                    exports.insert(statement_text.clone());
                }
                depth += change;
                if depth == 0 {
                    owner = None;
                }
                let _ = (start, statement_text);
                // A multi-line statement starts on one line and ends on a later
                // one; continue the scan from there rather than looking at the
                // continuation lines as fresh statements.
                index = next;
                continue;
            }
            if let Some((public, kind, head_name)) = item_head(stripped) {
                let start = index;
                let (statement_text, next, change) = statement(&lines, index, &mut state, false);
                let mut name = head_name.clone();
                if owner.is_some() && depth == 1 {
                    name = format!("{}::{name}", owner.as_deref().unwrap_or_default());
                }
                if kind == "mod" && statement_text.trim_end().ends_with(';') {
                    let child = if module == "main" || module == "lib" {
                        head_name.clone()
                    } else {
                        format!("{module}::{head_name}")
                    };
                    if known.contains(&child) {
                        declares.insert(child);
                    }
                }
                let raw: String = lines[start..next]
                    .iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                if public {
                    items.push(Item {
                        kind: kind.clone(),
                        name,
                        signature: raw.chars().take(240).collect(),
                        doc: preceding_docs(&lines, start),
                        line: start + 1,
                    });
                }
                if kind == "const"
                    && let Some(fact) = const_fact(&raw, &module, path, start + 1)
                {
                    facts.push(fact);
                }
                depth += change;
                if depth == 0 {
                    owner = None;
                }
                index = next;
                continue;
            }
        }
        let (_, change) = blank_and_delta(lines[index], &mut state);
        depth = (depth + change).max(0);
        if depth == 0 {
            owner = None;
        }
        facts.extend(env_facts_in(lines[index], path, index + 1));
        index += 1;
    }

    // A `use` is one edge; a fully qualified path elsewhere is another. Both
    // are dependency directions, and neither is implied by the other.
    let mut path_state = 0i32;
    for line in &lines[..test_start] {
        let (code, _) = blank_and_delta(line, &mut path_state);
        let trimmed = code.trim();
        if trimmed.starts_with("use ") || trimmed.starts_with("pub use ") {
            continue;
        }
        for reference in path_refs(&code) {
            for target in resolve(&reference, &module, known) {
                if target != module {
                    paths
                        .entry(target)
                        .or_default()
                        .insert(trimmed.chars().take(200).collect());
                }
            }
        }
    }

    Ok((
        Module {
            id: module,
            path: path.to_owned(),
            doc: module_doc(&lines),
            items,
            exports: exports.into_iter().collect(),
            declares: declares.into_iter().collect(),
            imports: imports
                .into_iter()
                .map(|(target, statements)| (target, statements.into_iter().collect()))
                .collect(),
            paths: paths
                .into_iter()
                .map(|(target, statements)| (target, statements.into_iter().collect()))
                .collect(),
            lines: lines.len(),
        },
        facts,
    ))
}

fn build_edges(modules: &[Module]) -> Vec<Value> {
    let mut edges: BTreeMap<(String, String, &'static str), BTreeSet<String>> = BTreeMap::new();
    for module in modules {
        for target in &module.declares {
            if target != &module.id {
                edges
                    .entry((module.id.clone(), target.clone(), "declares"))
                    .or_default()
                    .insert("mod".to_owned());
            }
        }
        for (kind, group) in [("uses", &module.imports), ("path", &module.paths)] {
            for (target, statements) in group {
                if target != &module.id {
                    edges
                        .entry((module.id.clone(), target.clone(), kind))
                        .or_default()
                        .extend(statements.iter().cloned());
                }
            }
        }
    }
    edges
        .into_iter()
        .map(|((from, to, kind), via)| {
            serde_json::json!({
                "from": from,
                "to": to,
                "kind": kind,
                "via": via.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// Topological layers from leaves upward. Containment is not a dependency:
/// a child using `super::` is normal and must not be read as a cycle.
fn build_layers(modules: &[Module], edges: &[Value]) -> (Vec<Vec<String>>, Vec<String>) {
    let ids: BTreeSet<String> = modules.iter().map(|module| module.id.clone()).collect();
    let mut dependencies: BTreeMap<String, BTreeSet<String>> =
        ids.iter().map(|id| (id.clone(), BTreeSet::new())).collect();
    for edge in edges {
        let kind = edge.get("kind").and_then(Value::as_str);
        if kind == Some("uses") || kind == Some("path") {
            let (Some(from), Some(to)) = (
                edge.get("from").and_then(Value::as_str),
                edge.get("to").and_then(Value::as_str),
            ) else {
                continue;
            };
            dependencies
                .entry(from.to_owned())
                .or_default()
                .insert(to.to_owned());
        }
    }
    let mut remaining = ids;
    let mut layers = Vec::new();
    let mut unresolved = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|id| dependencies[*id].is_disjoint(&remaining))
            .cloned()
            .collect();
        if ready.is_empty() {
            unresolved = remaining.into_iter().collect();
            break;
        }
        layers.push(ready.clone());
        for id in ready {
            remaining.remove(&id);
        }
    }
    (layers, unresolved)
}

fn map() -> Result<Map> {
    let settings = read_settings()?;
    let mut paths = BTreeSet::new();
    for root in &settings.roots {
        paths.extend(rust_files(&Path::new(ROOT).join(root))?);
    }
    let known: BTreeSet<String> = paths
        .iter()
        .map(|path| module_id(path, settings.roots.first().map_or("src", String::as_str)))
        .collect();

    let mut modules = Vec::new();
    let mut facts = Vec::new();
    for path in &paths {
        let root = settings
            .roots
            .iter()
            .find(|root| path.starts_with(&format!("{root}/")))
            .map_or("src", String::as_str);
        let (module, module_facts) = parse_file(path, root, &known)?;
        modules.push(module);
        facts.extend(module_facts);
    }
    modules.sort_by(|left, right| left.id.cmp(&right.id));

    let edges = build_edges(&modules);
    let (layers, unresolved) = build_layers(&modules, &edges);
    facts.sort_by(|left, right| left.id.cmp(&right.id));
    facts.dedup_by(|left, right| left.id == right.id);

    Ok(Map {
        modules,
        edges,
        layers,
        unresolved,
        facts,
        settings,
    })
}

fn artifact_json(map: &Map) -> String {
    let modules: Vec<Value> = map
        .modules
        .iter()
        .map(|module| {
            serde_json::json!({
                "id": module.id,
                "path": module.path,
                "doc": module.doc,
                "items": module.items.iter().map(|item| serde_json::json!({
                    "kind": item.kind,
                    "name": item.name,
                    "signature": item.signature,
                    "doc": item.doc,
                    "line": item.line,
                })).collect::<Vec<_>>(),
                "exports": module.exports,
                "declares": module.declares,
                "imports": module.imports,
                "paths": module.paths,
                "lines": module.lines,
            })
        })
        .collect();
    let facts: Vec<Value> = map
        .facts
        .iter()
        .map(|fact| {
            serde_json::json!({
                "id": fact.id,
                "value": fact.value,
                "source": fact.source,
                "excerpt": fact.excerpt,
            })
        })
        .collect();
    let artifact = serde_json::json!({
        "schema": "project-map/v1",
        "adapter": "rust",
        "entry_points": map.settings.entry_points,
        "modules": modules,
        "edges": map.edges,
        "layers": map.layers,
        "unresolved": map.unresolved,
        "facts": facts,
    });
    let mut text = serde_json::to_string_pretty(&artifact).unwrap_or_default();
    text.push('\n');
    text
}

/// Every `<!-- kind: target -->` in the document, with its line number.
fn citations(text: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("<!--") {
        let start = cursor + start;
        let Some(close) = text[start + 4..].find("-->") else {
            break;
        };
        let end = start + 4 + close;
        let inner = text[start + 4..end].trim();
        let line = text[..start].matches('\n').count() + 1;
        if let Some((kind, target)) = inner.split_once(':') {
            out.push((line, kind.trim().to_owned(), target.trim().to_owned()));
        }
        cursor = end + 3;
    }
    out
}

fn check_doc(path: &Path, map: &Map) -> Result<bool> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let modules: BTreeSet<&str> = map
        .modules
        .iter()
        .map(|module| module.id.as_str())
        .collect();
    let facts: BTreeMap<&str, &str> = map
        .facts
        .iter()
        .map(|fact| (fact.id.as_str(), fact.value.as_str()))
        .collect();
    let edges: BTreeSet<(String, String)> = map
        .edges
        .iter()
        .filter_map(|edge| {
            Some((
                edge.get("from")?.as_str()?.to_owned(),
                edge.get("to")?.as_str()?.to_owned(),
            ))
        })
        .collect();

    let mut cited_modules = BTreeSet::new();
    let mut cited_edges = BTreeSet::new();
    let mut cited_facts = BTreeSet::new();
    let mut violations = Vec::new();

    for (line, kind, target) in citations(&text) {
        match kind.as_str() {
            "module" => {
                if modules.contains(target.as_str()) {
                    cited_modules.insert(target);
                } else {
                    violations.push(format!(
                        "{}:{line}: module '{target}' is not in the map",
                        path.display()
                    ));
                }
            }
            "edge" => {
                let Some((from, to)) = target.split_once('>') else {
                    violations.push(format!(
                        "{}:{line}: edge '{target}' is not 'from>to'",
                        path.display()
                    ));
                    continue;
                };
                let pair = (from.to_owned(), to.to_owned());
                if edges.contains(&pair) {
                    cited_edges.insert(pair);
                } else {
                    violations.push(format!(
                        "{}:{line}: edge '{target}' is not in the map",
                        path.display()
                    ));
                }
            }
            "fact" => {
                let (name, expected) = target
                    .split_once('=')
                    .map_or((target.as_str(), ""), |(name, value)| {
                        (name.trim(), value.trim())
                    });
                let expected = expected.trim_start_matches('=').trim();
                match facts.get(name) {
                    Some(actual) if expected.is_empty() || expected == *actual => {
                        cited_facts.insert(name.to_owned());
                    }
                    Some(actual) => violations.push(format!(
                        "{}:{line}: fact '{name}' claims '{expected}', the code says '{actual}'",
                        path.display()
                    )),
                    None => violations.push(format!(
                        "{}:{line}: fact '{name}' is not in the map",
                        path.display()
                    )),
                }
            }
            other => violations.push(format!(
                "{}:{line}: unknown citation '{other}'",
                path.display()
            )),
        }
    }

    println!(
        "checked {} citations in {}",
        cited_modules.len() + cited_edges.len() + cited_facts.len(),
        path.display()
    );
    println!(
        "map: {} modules, {} edges ({} pairs), {} facts; cited: {} modules, {} edges, {} facts",
        map.modules.len(),
        map.edges.len(),
        edges.len(),
        map.facts.len(),
        cited_modules.len(),
        cited_edges.len(),
        cited_facts.len()
    );

    let mut warnings = 0;
    for fact in &map.facts {
        if cited_facts.contains(&fact.id) {
            continue;
        }
        let name = fact.id.rsplit("::").next().unwrap_or("");
        let needles = [fact.value.as_str(), name];
        if needles
            .iter()
            .any(|needle| needle.len() >= 5 && text.contains(needle))
        {
            warnings += 1;
            if warnings <= 20 {
                println!(
                    "  uncited: fact {} appears in the doc without a citation",
                    fact.id
                );
            }
        }
    }
    for module in &map.modules {
        if cited_modules.contains(&module.id) {
            continue;
        }
        let stem = Path::new(&module.path)
            .file_stem()
            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
        if text.contains(&format!("{stem}.rs")) {
            warnings += 1;
            if warnings <= 20 {
                println!(
                    "  uncited: module {} is named in the doc without a citation",
                    module.id
                );
            }
        }
    }
    if warnings > 20 {
        println!("  ... and {} more uncited mentions", warnings - 20);
    }

    for violation in &violations {
        eprintln!("ERROR {violation}");
    }
    Ok(violations.is_empty())
}

fn run() -> Result<i32> {
    let args = parse_args()?;
    let map = map()?;
    let artifact = artifact_json(&map);
    let doc = args
        .doc
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(ROOT).join(&map.settings.doc));

    if args.check {
        return Ok(if check_doc(&doc, &map)? { 0 } else { 1 });
    }
    if args.stdout {
        print!("{artifact}");
        return Ok(0);
    }

    let out = args
        .out
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(ROOT).join(&map.settings.artifact));
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&out, &artifact).with_context(|| format!("writing {}", out.display()))?;
    println!("wrote {}", out.strip_prefix(ROOT).unwrap_or(&out).display());
    println!(
        "{} modules, {} edges, {} layers, {} facts, {} unresolved",
        map.modules.len(),
        map.edges.len(),
        map.layers.len(),
        map.facts.len(),
        map.unresolved.len()
    );
    Ok(0)
}

fn main() -> Result<()> {
    std::process::exit(run()?);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_or_comment_cannot_create_a_path_edge() {
        let lines = [
            r#"let a = crate::config::env_or("X", 1);"#,
            r#"let s = "crate::fake::InString";"#,
            "let r = r#\"crate::fake::Raw\"#;",
            "/// [`crate::fake::Doc`]",
            "//! crate::fake::ModuleDoc",
        ];
        let mut state = 0;
        let mut found = Vec::new();
        for line in lines {
            let (code, _) = blank_and_delta(line, &mut state);
            found.extend(path_refs(&code));
        }
        assert_eq!(found, vec!["crate::config::env_or"]);
    }

    #[test]
    fn a_grouped_use_of_one_module_resolves_to_that_module() {
        let known = BTreeSet::from(["tools".to_owned(), "tools::edit".to_owned()]);
        let targets = resolve(
            "crate::tools::{AgentContext, ToolResult}",
            "tools::edit",
            &known,
        );
        assert!(targets.contains("tools"), "{targets:?}");
    }

    #[test]
    fn module_ids_drop_the_source_root_and_mod_rs() {
        assert_eq!(module_id("src/tools/edit.rs", "src"), "tools::edit");
        assert_eq!(module_id("src/claude/mod.rs", "src"), "claude");
        assert_eq!(module_id("src/main.rs", "src"), "main");
    }
}
