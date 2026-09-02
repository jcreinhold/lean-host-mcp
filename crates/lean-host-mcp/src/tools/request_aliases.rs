//! Forgiving decoding of semantic tool requests.
//!
//! Agents write requests from the tool descriptions and guess the rest, and six
//! weeks of recorded calls show the same guesses recurring: `file` instead of a
//! `target` object, `command` for `commands`, `declaration` for `name`, a
//! verification group tagged `kind: "file"` instead of `file_all`, a bare
//! string where a proof-position selector object is expected. Each guess costs
//! a round trip, and for a client with an idle timeout sometimes the whole
//! call. This module maps the recurring synonyms onto the canonical schema
//! before typed decoding so that the first call succeeds.
//!
//! The canonical schema is unchanged: synonyms are accepted on input, never
//! advertised as the way to write a request, and never emitted. When a
//! canonical field is present the synonyms are dropped rather than allowed to
//! override it.

use serde_json::{Map, Value, json};

/// The canonical mode for a `kind` the caller wrote, including the synonyms
/// agents reach for. Unknown kinds pass through unchanged so the caller still
/// receives the `invalid_kind` error with the allowed list.
pub(crate) fn canonical_kind<'a>(tool: &str, kind: &'a str) -> &'a str {
    match (tool, kind) {
        ("lean_context", "goal" | "goals" | "state" | "proof_state" | "position" | "proof") => "proof_position",
        ("lean_trial", "tactic" | "tactics" | "step" | "proof" | "try" | "attempt") => "proof_step",
        ("lean_trial", "snippet" | "check" | "eval" | "elab" | "elaborate" | "commands" | "run") => "command",
        ("lean_lookup", "signature" | "print" | "decl" | "name" | "inspect" | "check" | "constant" | "lookup") => {
            "declaration"
        }
        ("lean_lookup", "inventory" | "outline" | "list" | "list_declarations") => "declarations",
        ("lean_lookup", "usages" | "uses" | "find_references" | "refs") => "references",
        ("lean_lookup", "suggest" | "premises" | "library_search") => "proof_search",
        ("lean_status", "health" | "capabilities" | "toolchain" | "version" | "info" | "status" | "config") => {
            "project"
        }
        ("lean_status", "diagnostics" | "file" | "check" | "errors" | "elaborate") => "file_diagnostics",
        _ => kind,
    }
}

/// Guidance for a `kind` that names something another tool does, or something
/// no tool does, so the `invalid_kind` error says where to go instead of only
/// listing the allowed modes.
pub(crate) fn cross_tool_hint(tool: &str, kind: &str) -> Option<&'static str> {
    match (tool, kind) {
        ("lean_lookup", "search" | "find" | "query" | "grep" | "names" | "name_search" | "fuzzy") => Some(
            "lean_lookup has no name search. Use kind=\"declaration\" with the exact name (it resolves in the file's \
             environment when `file` is given), kind=\"declarations\" to list what a file or module defines, or \
             kind=\"proof_search\" for goal-directed candidates.",
        ),
        ("lean_trial" | "lean_lookup" | "lean_context", "file" | "diagnostics" | "file_diagnostics" | "errors") => {
            Some("To elaborate a whole file and read its diagnostics, call lean_status with kind=\"file_diagnostics\".")
        }
        ("lean_context" | "lean_trial" | "lean_status", "declaration" | "lookup" | "signature" | "print") => {
            Some("Declaration inspection is lean_lookup with kind=\"declaration\".")
        }
        ("lean_trial" | "lean_lookup" | "lean_status" | "lean_context", "verify" | "verification") => {
            Some("Verification is lean_verify; it takes `targets` groups and no `kind`.")
        }
        ("lean_lookup" | "lean_status" | "lean_context", "command" | "eval" | "check") => {
            Some("Running Lean commands such as #check is lean_trial with kind=\"command\".")
        }
        ("lean_lookup" | "lean_status", "proof_step" | "tactic" | "step") => {
            Some("Trying a tactic at a proof position is lean_trial with kind=\"proof_step\".")
        }
        _ => None,
    }
}

/// Rewrite the mode-specific fields of one request onto the canonical names.
pub(crate) fn normalize_args(tool: &str, kind: &str, args: &mut Map<String, Value>) {
    match (tool, kind) {
        ("lean_context", "proof_position") => {
            rename_missing(args, "file", &["path", "source", "filename"]);
            rename_missing(args, "declaration", &["decl", "name", "theorem", "lemma", "definition"]);
            normalize_proof_position(args);
        }
        ("lean_trial", "proof_step") => {
            rename_missing(args, "file", &["path", "source", "filename"]);
            rename_missing(args, "declaration", &["decl", "name", "theorem", "lemma", "definition"]);
            normalize_proof_position(args);
            if !args.contains_key("snippet") && !args.contains_key("snippets") {
                if let Some(value) = take_first(
                    args,
                    &["tactic", "tactics", "text", "step", "proof", "candidate", "candidates"],
                ) {
                    let key = if matches!(value, Value::Array(_)) {
                        "snippets"
                    } else {
                        "snippet"
                    };
                    args.insert(key.to_owned(), value);
                }
            } else if matches!(args.get("snippet"), Some(Value::Array(_)))
                && let Some(value) = args.remove("snippet")
            {
                args.entry("snippets".to_owned()).or_insert(value);
            }
        }
        ("lean_trial", "command") => {
            rename_missing(args, "file", &["path", "filename"]);
            rename_missing(
                args,
                "commands",
                &["command", "code", "snippet", "source", "text", "lean", "input"],
            );
            if let Some(Value::Array(lines)) = args.get("commands") {
                let joined = lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
                args.insert("commands".to_owned(), Value::String(joined));
            }
        }
        ("lean_lookup", "declaration") => {
            rename_missing(
                args,
                "name",
                &["declaration", "decl", "constant", "symbol", "identifier", "ident"],
            );
            rename_missing(args, "file", &["path", "filename"]);
            if let Some(Value::Array(fields)) = args.get_mut("fields") {
                for field in fields.iter_mut() {
                    if let Value::String(name) = field {
                        let canonical = match name.as_str() {
                            "value" | "body" | "definition" | "term" | "proof" => "source",
                            "type" | "signature" | "sig" => "statement",
                            "doc" | "docs" | "documentation" => "docstring",
                            "attrs" => "attributes",
                            other => other,
                        };
                        if canonical != name {
                            *name = canonical.to_owned();
                        }
                    }
                }
            }
        }
        ("lean_lookup", "declarations") => {
            if !args.contains_key("target") {
                if let Some(path) = take_first(args, &["file", "path", "source", "filename"]) {
                    args.insert("target".to_owned(), json!({ "kind": "file", "path": path }));
                } else if let Some(module) = take_first(args, &["module", "module_name"]) {
                    args.insert("target".to_owned(), json!({ "kind": "module", "module": module }));
                }
            } else if let Some(Value::String(name)) = args.get("target") {
                let target = if is_lean_file_name(name) {
                    json!({ "kind": "file", "path": name })
                } else {
                    json!({ "kind": "module", "module": name })
                };
                args.insert("target".to_owned(), target);
            }
            if let Some(Value::Object(target)) = args.get_mut("target") {
                rename_missing(target, "path", &["file", "source", "filename"]);
                if !target.contains_key("kind") {
                    let kind = if target.contains_key("module") {
                        "module"
                    } else {
                        "file"
                    };
                    target.insert("kind".to_owned(), Value::String(kind.to_owned()));
                }
            }
        }
        ("lean_lookup", "references") => {
            rename_missing(args, "name", &["declaration", "decl", "constant", "symbol"]);
            rename_missing(args, "file", &["path", "filename"]);
        }
        ("lean_lookup", "proof_search") => {
            rename_missing(args, "file", &["path", "filename"]);
            rename_missing(args, "declaration", &["decl", "name", "theorem", "lemma"]);
            rename_missing(args, "goal", &["goal_text", "target", "query"]);
        }
        ("lean_status", "file_diagnostics") => {
            rename_missing(args, "file", &["path", "source", "filename"]);
        }
        _ => {}
    }
}

/// Rewrite a raw `lean_verify` request onto `targets` groups with canonical
/// group kinds. Accepts a single group at top level (`file` with
/// `declarations`, `file` alone, or `module` alone), a singular `target`, a
/// `kind` naming the group kind, string groups, and the group-kind synonyms
/// `file`, `module`, and `declarations`.
pub(crate) fn normalize_verify_request(value: &mut Value) {
    let Value::Object(args) = value else {
        return;
    };
    let kind_shorthand = args
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned);
    if let Some(kind) = kind_shorthand {
        let group_kind = canonical_target_kind(&kind);
        if group_kind.is_some() || matches!(kind.as_str(), "verify" | "targets" | "verification") {
            args.remove("kind");
            if let Some(group_kind) = group_kind
                && !args.contains_key("targets")
            {
                let mut group = take_group_fields(args);
                group.insert("kind".to_owned(), Value::String(group_kind.to_owned()));
                args.insert("targets".to_owned(), Value::Array(vec![Value::Object(group)]));
            }
        }
    }
    if !args.contains_key("targets") {
        if let Some(target) = take_first(args, &["target", "group", "groups"]) {
            args.insert("targets".to_owned(), into_array(target));
        } else if ["file", "path", "module"].iter().any(|key| args.contains_key(*key)) {
            let group = take_group_fields(args);
            args.insert("targets".to_owned(), Value::Array(vec![Value::Object(group)]));
        }
    }
    if matches!(args.get("targets"), Some(Value::Object(_) | Value::String(_)))
        && let Some(single) = args.remove("targets")
    {
        args.insert("targets".to_owned(), Value::Array(vec![single]));
    }
    if let Some(Value::Array(groups)) = args.get_mut("targets") {
        for group in groups.iter_mut() {
            normalize_target_group(group);
        }
    }
}

fn take_group_fields(args: &mut Map<String, Value>) -> Map<String, Value> {
    let mut group = Map::new();
    for key in [
        "file",
        "path",
        "module",
        "declarations",
        "declaration",
        "names",
        "name",
        "decls",
    ] {
        if let Some(value) = args.remove(key) {
            group.insert(key.to_owned(), value);
        }
    }
    group
}

/// Whether a bare string target names a Lean source file rather than a module, by its extension ignoring case.
fn is_lean_file_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lean"))
}

/// Wrap a scalar in a one-element array; arrays pass through.
fn into_array(value: Value) -> Value {
    if matches!(value, Value::Array(_)) {
        value
    } else {
        Value::Array(vec![value])
    }
}

fn canonical_target_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "explicit" | "declarations" | "declaration" | "names" | "decls" | "explicit_declarations" => Some("explicit"),
        "file_all" | "file" | "source" | "path" | "whole_file" => Some("file_all"),
        "module_all" | "module" | "mod" | "whole_module" => Some("module_all"),
        _ => None,
    }
}

fn normalize_target_group(group: &mut Value) {
    match group {
        Value::String(name) => {
            *group = if is_lean_file_name(name) {
                json!({ "kind": "file_all", "file": name })
            } else {
                json!({ "kind": "module_all", "module": name })
            };
        }
        Value::Object(map) => {
            rename_missing(map, "file", &["path", "source", "filename"]);
            rename_missing(map, "module", &["mod", "module_name"]);
            if !map.contains_key("declarations") {
                if let Some(value) = take_first(map, &["declaration", "names", "name", "decls", "decl"]) {
                    map.insert("declarations".to_owned(), into_array(value));
                }
            } else if matches!(map.get("declarations"), Some(Value::String(_)))
                && let Some(single) = map.remove("declarations")
            {
                map.insert("declarations".to_owned(), Value::Array(vec![single]));
            }
            // An explicit unknown kind is left in place so the decoder's error names it.
            let kind = match map.get("kind").and_then(Value::as_str) {
                Some(kind) => canonical_target_kind(kind),
                None if map.contains_key("declarations") => Some("explicit"),
                None if map.contains_key("module") => Some("module_all"),
                None if map.contains_key("file") => Some("file_all"),
                None => None,
            };
            if let Some(kind) = kind {
                map.insert("kind".to_owned(), Value::String(kind.to_owned()));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {}
    }
}

fn normalize_proof_position(args: &mut Map<String, Value>) {
    rename_missing(
        args,
        "proof_position",
        &["position", "selector", "at", "proof_state", "where"],
    );
    let Some(value) = args.get_mut("proof_position") else {
        return;
    };
    match value {
        Value::String(text) => {
            let lowered = text.trim().to_ascii_lowercase();
            *value = match lowered.as_str() {
                "" | "default" | "start" | "entry" | "begin" | "beginning" | "initial" | "none" => {
                    json!({ "kind": "default" })
                }
                _ => json!({ "kind": "after_text", "text": text.clone() }),
            };
        }
        Value::Number(number) => {
            if let Some(index) = number.as_u64() {
                *value = json!({ "kind": "index", "index": index });
            }
        }
        Value::Object(selector) => {
            rename_missing(selector, "text", &["tactic", "after", "fragment", "match"]);
            rename_missing(selector, "index", &["step", "nth", "n"]);
            let canonical = selector
                .get("kind")
                .and_then(Value::as_str)
                .map(|kind| match kind {
                    "start" | "entry" | "begin" | "beginning" | "initial" | "none" => "default",
                    "after" | "text" | "tactic" | "fragment" => "after_text",
                    "at_index" | "nth" | "step" | "position" => "index",
                    other => other,
                })
                .map(str::to_owned)
                .or_else(|| {
                    if selector.contains_key("text") {
                        Some("after_text".to_owned())
                    } else if selector.contains_key("index") {
                        Some("index".to_owned())
                    } else {
                        Some("default".to_owned())
                    }
                });
            if let Some(kind) = canonical {
                selector.insert("kind".to_owned(), Value::String(kind));
            }
        }
        Value::Null | Value::Bool(_) | Value::Array(_) => {}
    }
}

fn take_first(args: &mut Map<String, Value>, names: &[&str]) -> Option<Value> {
    names.iter().find_map(|name| args.remove(*name))
}

/// Move the first present synonym onto `canonical` when it is absent; when the
/// canonical field is present, drop the synonyms so they cannot shadow it.
fn rename_missing(args: &mut Map<String, Value>, canonical: &str, aliases: &[&str]) {
    if args.contains_key(canonical) {
        for alias in aliases {
            args.remove(*alias);
        }
        return;
    }
    if let Some(value) = take_first(args, aliases) {
        args.insert(canonical.to_owned(), value);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn field(map: &Map<String, Value>, key: &str) -> Value {
        map.get(key).cloned().unwrap_or(Value::Null)
    }

    fn at(value: &Value, key: &str) -> Value {
        value.get(key).cloned().unwrap_or(Value::Null)
    }

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_)) => {
                panic!("expected an object, got {other}")
            }
        }
    }

    #[test]
    fn declarations_accept_a_bare_file_or_module_in_place_of_target() {
        let mut args = object(json!({ "file": "Proofs/A.lean", "limit": 50 }));
        normalize_args("lean_lookup", "declarations", &mut args);
        assert_eq!(
            field(&args, "target"),
            json!({ "kind": "file", "path": "Proofs/A.lean" })
        );
        assert!(!args.contains_key("file"));

        let mut args = object(json!({ "module": "Proofs.A" }));
        normalize_args("lean_lookup", "declarations", &mut args);
        assert_eq!(
            field(&args, "target"),
            json!({ "kind": "module", "module": "Proofs.A" })
        );

        let mut args = object(json!({ "target": "Proofs/A.lean" }));
        normalize_args("lean_lookup", "declarations", &mut args);
        assert_eq!(
            field(&args, "target"),
            json!({ "kind": "file", "path": "Proofs/A.lean" })
        );

        let mut args = object(json!({ "target": { "file": "Proofs/A.lean" } }));
        normalize_args("lean_lookup", "declarations", &mut args);
        assert_eq!(
            field(&args, "target"),
            json!({ "kind": "file", "path": "Proofs/A.lean" })
        );
    }

    #[test]
    fn declaration_accepts_declaration_for_name_and_maps_field_synonyms() {
        let mut args = object(json!({ "declaration": "Nat.add", "fields": ["value", "type", "docs"] }));
        normalize_args("lean_lookup", "declaration", &mut args);
        assert_eq!(field(&args, "name"), json!("Nat.add"));
        assert_eq!(field(&args, "fields"), json!(["source", "statement", "docstring"]));
    }

    #[test]
    fn canonical_fields_win_over_synonyms() {
        let mut args = object(json!({ "name": "Nat.add", "declaration": "Nat.mul" }));
        normalize_args("lean_lookup", "declaration", &mut args);
        assert_eq!(field(&args, "name"), json!("Nat.add"));
        assert!(!args.contains_key("declaration"));
    }

    #[test]
    fn command_accepts_command_code_and_line_arrays() {
        let mut args = object(json!({ "command": "#check Nat" }));
        normalize_args("lean_trial", "command", &mut args);
        assert_eq!(field(&args, "commands"), json!("#check Nat"));

        let mut args = object(json!({ "code": ["#check Nat", "#check Int"] }));
        normalize_args("lean_trial", "command", &mut args);
        assert_eq!(field(&args, "commands"), json!("#check Nat\n#check Int"));
    }

    #[test]
    fn proof_step_accepts_tactic_candidates_and_loose_positions() {
        let mut args = object(json!({ "file": "A.lean", "decl": "foo", "tactic": "simp", "position": "start" }));
        normalize_args("lean_trial", "proof_step", &mut args);
        assert_eq!(field(&args, "declaration"), json!("foo"));
        assert_eq!(field(&args, "snippet"), json!("simp"));
        assert_eq!(field(&args, "proof_position"), json!({ "kind": "default" }));

        let mut args = object(json!({ "file": "A.lean", "declaration": "foo", "candidates": ["simp", "ring"] }));
        normalize_args("lean_trial", "proof_step", &mut args);
        assert_eq!(field(&args, "snippets"), json!(["simp", "ring"]));

        let mut args =
            object(json!({ "file": "A.lean", "declaration": "foo", "snippet": "rfl", "proof_position": "intro h" }));
        normalize_args("lean_trial", "proof_step", &mut args);
        assert_eq!(
            field(&args, "proof_position"),
            json!({ "kind": "after_text", "text": "intro h" })
        );

        let mut args = object(json!({ "file": "A.lean", "declaration": "foo", "snippet": "rfl", "proof_position": 2 }));
        normalize_args("lean_trial", "proof_step", &mut args);
        assert_eq!(field(&args, "proof_position"), json!({ "kind": "index", "index": 2 }));

        let mut args =
            object(json!({ "file": "A.lean", "declaration": "foo", "proof_position": { "tactic": "exact h" } }));
        normalize_args("lean_context", "proof_position", &mut args);
        assert_eq!(
            field(&args, "proof_position"),
            json!({ "kind": "after_text", "text": "exact h" })
        );
    }

    #[test]
    fn verify_accepts_one_group_at_top_level() {
        let mut value = json!({ "file": "Proofs/A.lean", "declarations": ["A.foo"], "allow_sorry": true });
        normalize_verify_request(&mut value);
        assert_eq!(
            value,
            json!({
                "targets": [{ "kind": "explicit", "file": "Proofs/A.lean", "declarations": ["A.foo"] }],
                "allow_sorry": true
            })
        );

        let mut value = json!({ "file": "Proofs/A.lean", "declaration": "A.foo" });
        normalize_verify_request(&mut value);
        assert_eq!(
            at(&value, "targets"),
            json!([{ "kind": "explicit", "file": "Proofs/A.lean", "declarations": ["A.foo"] }])
        );

        let mut value = json!({ "file": "Proofs/A.lean" });
        normalize_verify_request(&mut value);
        assert_eq!(
            at(&value, "targets"),
            json!([{ "kind": "file_all", "file": "Proofs/A.lean" }])
        );

        let mut value = json!({ "module": "Proofs.A" });
        normalize_verify_request(&mut value);
        assert_eq!(
            at(&value, "targets"),
            json!([{ "kind": "module_all", "module": "Proofs.A" }])
        );
    }

    #[test]
    fn verify_accepts_group_kind_synonyms_and_kind_shorthand() {
        let mut value = json!({ "targets": [
            { "kind": "file", "path": "Proofs/A.lean" },
            { "kind": "module", "module": "Proofs.B" },
            { "kind": "declarations", "file": "Proofs/C.lean", "declarations": "C.foo" },
            "Proofs.D",
            "Proofs/E.lean"
        ] });
        normalize_verify_request(&mut value);
        assert_eq!(
            at(&value, "targets"),
            json!([
                { "kind": "file_all", "file": "Proofs/A.lean" },
                { "kind": "module_all", "module": "Proofs.B" },
                { "kind": "explicit", "file": "Proofs/C.lean", "declarations": ["C.foo"] },
                { "kind": "module_all", "module": "Proofs.D" },
                { "kind": "file_all", "file": "Proofs/E.lean" }
            ])
        );

        let mut value = json!({ "kind": "file_all", "file": "Proofs/A.lean" });
        normalize_verify_request(&mut value);
        assert_eq!(
            value,
            json!({ "targets": [{ "kind": "file_all", "file": "Proofs/A.lean" }] })
        );

        let mut value = json!({ "target": { "kind": "explicit", "file": "A.lean", "declarations": ["x"] } });
        normalize_verify_request(&mut value);
        assert_eq!(
            value,
            json!({ "targets": [{ "kind": "explicit", "file": "A.lean", "declarations": ["x"] }] })
        );

        // An unknown kind is left for the caller to see in the error.
        let mut value = json!({ "kind": "bogus", "targets": [] });
        normalize_verify_request(&mut value);
        assert_eq!(at(&value, "kind"), json!("bogus"));

        let mut value = json!({ "targets": [{ "kind": "bogus_group", "file": "A.lean" }] });
        normalize_verify_request(&mut value);
        assert_eq!(
            at(&value, "targets"),
            json!([{ "kind": "bogus_group", "file": "A.lean" }])
        );
    }

    #[test]
    fn kind_synonyms_and_cross_tool_hints() {
        assert_eq!(canonical_kind("lean_lookup", "signature"), "declaration");
        assert_eq!(canonical_kind("lean_status", "health"), "project");
        assert_eq!(canonical_kind("lean_trial", "tactic"), "proof_step");
        assert_eq!(canonical_kind("lean_context", "goal"), "proof_position");
        assert_eq!(canonical_kind("lean_lookup", "search"), "search");
        assert!(
            cross_tool_hint("lean_lookup", "search")
                .unwrap()
                .contains("no name search")
        );
        assert!(
            cross_tool_hint("lean_trial", "file")
                .unwrap()
                .contains("file_diagnostics")
        );
        assert!(cross_tool_hint("lean_lookup", "declaration").is_none());
    }
}
