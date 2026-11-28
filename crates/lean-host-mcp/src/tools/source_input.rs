//! Shared Lean source-file ingestion for proof-agent tools.
//!
//! **One [`read_query_file`] per handler.** A [`QueryFile`] is the currency
//! every downstream helper takes; nothing below a handler re-derives one from a
//! path. Two reasons, in order of weight:
//!
//! 1. *Correctness.* The bytes, the content hash, and the header-import scan
//!    must describe one snapshot. A second read can see an edit the first did
//!    not, which would put a cache key, an elaboration, and a reported import
//!    list out of step with each other — a disagreement no caller could detect.
//! 2. *Cost.* Measured on this machine (release): **60 µs** for a 3.6 KB file
//!    and **381 µs** for a 191 KB one — roughly 57 µs of fixed path
//!    canonicalization and syscalls plus ~1.7 µs/KiB of read, hash, and header
//!    scan. That is invisible next to an elaboration, but a second read costs
//!    *more than the entire* warm module-query cache hit it would precede,
//!    which the repo targets at under 50 µs. Handing the struct down instead
//!    trades those syscalls for a memcpy of the source.
//!
//! `lean_verify`'s `file_all` group is the one place a file is still read twice,
//! because the two reads are in different *handlers*: it invokes
//! `declaration_inventory` as a tool and then prepares its own verify group.
//! Removing it would mean threading a `QueryFile` out through a public tool
//! response, and the duplicate is followed by elaborating every declaration the
//! inventory found — sub-millisecond against a path that is tens of
//! milliseconds at best. Not worth the surface.

use std::path::{Path, PathBuf};

use crate::error::{Result, ServerError};

/// One snapshot of a Lean source file: the bytes, their content hash, and the
/// header-import scan of those same bytes. Produced only by
/// [`read_query_file`], so the three can never describe different reads.
pub(crate) struct QueryFile {
    pub resolved: PathBuf,
    pub hash: [u8; 32],
    pub imports: Vec<String>,
    pub source: String,
}

pub(crate) fn read_query_file(root: &Path, path: &Path) -> Result<QueryFile> {
    let resolved = resolve_path(root, path).canonicalize().map_err(ServerError::Io)?;
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = crate::cache::hash_bytes(&bytes);
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    let imports = header_imports(&source);
    Ok(QueryFile {
        resolved,
        hash,
        imports,
        source,
    })
}

pub(crate) fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// The modules a Lean file's header imports, each spelled as in the file.
///
/// It reads the header as Lean's parser does (`Lean.Parser.Module.header`): an
/// optional `module`, an optional `prelude`, then `import` commands, each with
/// optional `public` and `meta` before the keyword and `all` after it. Any
/// other word ends the header, and so does a doc comment, `/--` or `/-!`,
/// which Lean reads as a token. It leaves out the `Init` that Lean imports
/// into a file without `prelude`.
pub(crate) fn header_imports(source: &str) -> Vec<String> {
    let mut words = HeaderWords(source).peekable();
    words.next_if_eq(&"module");
    words.next_if_eq(&"prelude");
    let mut imports = Vec::new();
    loop {
        words.next_if_eq(&"public");
        words.next_if_eq(&"meta");
        if words.next_if_eq(&"import").is_none() {
            return imports;
        }
        words.next_if_eq(&"all");
        let Some(module) = words.next() else {
            return imports;
        };
        imports.push(module.to_owned());
    }
}

/// The words at the start of a file, with the whitespace and comments between
/// them skipped. They end at the first token that is not a word, such as a doc
/// comment.
struct HeaderWords<'a>(&'a str);

impl<'a> Iterator for HeaderWords<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let rest = skip_blank(self.0);
        let (word, rest) = rest.split_at(word_end(rest));
        self.0 = rest;
        (!word.is_empty()).then_some(word)
    }
}

/// `text` without the whitespace, `--` comments and `/- -/` comments at its
/// start. Like Lean, it skips the character after a comment's `/-` before it
/// looks for the end, so `/-/- x -/` is one comment.
fn skip_blank(mut text: &str) -> &str {
    loop {
        text = text.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(comment) = text.strip_prefix("--") {
            text = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(body) = text.strip_prefix("/-")
            && !body.starts_with(['-', '!'])
        {
            let mut body = body.chars();
            body.next();
            text = after_block_comment(body.as_str());
        } else {
            return text;
        }
    }
}

/// The text after the `-/` that closes a block comment whose body starts
/// `body`, counting nested `/-` and `-/`, or nothing if it never closes.
fn after_block_comment(mut body: &str) -> &str {
    let mut depth = 1_usize;
    while let Some(at) = body.find(['-', '/']) {
        let tail = body.split_at(at).1;
        if let Some(after) = tail.strip_prefix("-/") {
            if depth == 1 {
                return after;
            }
            depth = depth.saturating_sub(1);
            body = after;
        } else if let Some(after) = tail.strip_prefix("/-") {
            depth = depth.saturating_add(1);
            body = after;
        } else {
            body = tail.get(1..).unwrap_or_default();
        }
    }
    ""
}

/// The length in bytes of the word that starts `text`. A word stops at
/// whitespace or at a `-` or `/`, which no identifier holds outside `«»`.
fn word_end(text: &str) -> usize {
    let mut quoted = false;
    for (at, c) in text.char_indices() {
        match c {
            '«' => quoted = true,
            '»' => quoted = false,
            ' ' | '\t' | '\r' | '\n' | '-' | '/' if !quoted => return at,
            _ => {}
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::header_imports;

    #[test]
    fn header_imports_handles_public_meta_and_all() {
        let source = "\
module

public import Foo.Bar
meta import Baz.Qux
import all Project.Internal
import Init -- comment
";
        assert_eq!(
            header_imports(source),
            ["Foo.Bar", "Baz.Qux", "Project.Internal", "Init"]
        );
    }

    #[test]
    fn header_imports_skips_a_copyright_block_before_module() {
        let source = "\
/-
Copyright (c) 2026 Example. All rights reserved.
Released under Apache 2.0 license.
-/
module

prelude
public import Foo.Bar
";
        assert_eq!(header_imports(source), ["Foo.Bar"]);
    }

    #[test]
    fn header_imports_skips_a_comment_inside_a_line() {
        assert_eq!(
            header_imports("/- x -/ import A/- y -/import B--z\nimport C"),
            ["A", "B", "C"]
        );
    }

    #[test]
    fn header_imports_skips_nested_block_comments() {
        let source = "\
/- outer
/- inner -/
import Hidden
-/
import Shown
";
        assert_eq!(header_imports(source), ["Shown"]);
        assert_eq!(header_imports("/-/- one comment, as Lean reads it -/\nimport A"), ["A"]);
    }

    #[test]
    fn header_imports_stops_at_the_first_token_after_the_header() {
        let source = "\
import A

/-- Uses
import B
-/
def x := 0
";
        assert_eq!(header_imports(source), ["A"]);
        assert_eq!(header_imports("import A\n/-! import B -/"), ["A"]);
        assert_eq!(header_imports("import A\npublic section\nimport B"), ["A"]);
        assert_eq!(header_imports("import «my-lib».«A B»"), ["«my-lib».«A B»"]);
    }
}
