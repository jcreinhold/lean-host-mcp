//! How Lake v4.36 reads a step's words, as far as the server must know it.
//!
//! A step is the list of words after `lake`. Lake reads the first part as
//! its own options, command and targets. After a literal `--`, or after the
//! word that follows `exe`, `exec`, `env` or `run`, it passes the rest to the
//! program unread. The server refuses options in Lake's part that move Lake
//! to another project (`-d`, `-f`, `--dir`, `--file`), and refuses a client
//! word that starts with `-` anywhere Lake would read it as an option.

/// Commands whose next word names a program; Lake passes the words after it
/// through unread.
const PASS_THROUGH: [&str; 4] = ["exe", "exec", "env", "run"];

/// Short options that take the next word as their value.
const SHORT_WITH_VALUE: [char; 4] = ['d', 'f', 'o', 'K'];

/// Long options that take the next word as their value, unless written
/// `--name=value`.
const LONG_WITH_VALUE: [&str; 23] = [
    "--package",
    "--service",
    "--scope",
    "--repo",
    "--platform",
    "--toolchain",
    "--rev",
    "--max-revs",
    "--log-level",
    "--fail-level",
    "--packages",
    "--dir",
    "--file",
    "--linters",
    "--lint-only",
    "--checks",
    "--only",
    "--add-downstream",
    "--add-public-downstream",
    "--config",
    "--from-export",
    "--challenge-from-export",
    "--solution-from-export",
];

/// Whether Lake reads `word` as an option: a dash and at least one more
/// character.
fn is_option(word: &str) -> bool {
    word.starts_with('-') && word.len() > 1
}

/// Check one option word in Lake's part, and say whether it takes the next
/// word as its value.
fn read_option(word: &str) -> Result<bool, String> {
    if word.starts_with("--") {
        let name = word.split(['=', ' ']).next().unwrap_or(word);
        if name == "--dir" || name == "--file" {
            return Err(format!("`{word}` would move Lake to another project"));
        }
        return Ok(name.len() == word.len() && LONG_WITH_VALUE.contains(&name));
    }
    let mut chars = word.chars().skip(1);
    let short = chars.next().unwrap_or('-');
    if short == 'd' || short == 'f' {
        return Err(format!("`{word}` would move Lake to another project"));
    }
    match chars.next() {
        None => Ok(SHORT_WITH_VALUE.contains(&short)),
        // `-x=value` and `-x value` in one word.
        Some('=' | ' ') => Ok(false),
        // `-xvalue` for an option that takes a value.
        Some(_) if SHORT_WITH_VALUE.contains(&short) => Ok(false),
        // Lake reads `-xrest` as `-x` followed by the word `rest`.
        Some(_) => Err(format!("write each short option in `{word}` as its own word")),
    }
}

/// Check `words` as Lake reads them. Words from index `client` on came from
/// the client. Returns where the part Lake passes through starts, if it does.
///
/// # Errors
/// Names the first word Lake would read in a way the server refuses.
pub(super) fn check_step(words: &[String], client: usize) -> Result<Option<usize>, String> {
    let mut command: Option<&str> = None;
    let mut index = 0_usize;
    while let Some(word) = words.get(index) {
        let next = index.saturating_add(1);
        if word.is_empty() {
            return Err("a step word is empty".to_owned());
        }
        if word.contains('\0') {
            return Err(format!("`{}` contains NUL", word.escape_default()));
        }
        if is_option(word) {
            if index >= client {
                return Err(format!(
                    "argument `{word}` starts with `-`; Lake would read it as an option. Arguments that start with `-` \
                     fit only after `exe <name>` or `--`"
                ));
            }
            if word == "--" {
                return Ok(Some(next));
            }
            if read_option(word)? {
                if words.get(next).is_none() {
                    return Err(format!("`{word}` ends the step but takes a value"));
                }
                index = next.saturating_add(1);
            } else {
                index = next;
            }
            continue;
        }
        if command.is_some_and(|command| PASS_THROUGH.contains(&command)) {
            return Ok(Some(next));
        }
        command.get_or_insert(word);
        index = next;
    }
    Ok(None)
}

/// Whether `words` name a Lake command, not only options.
pub(super) fn has_command(words: &[String]) -> bool {
    let mut index = 0_usize;
    while let Some(word) = words.get(index) {
        if word == "--" {
            return false;
        }
        if !is_option(word) {
            return true;
        }
        let takes_value = read_option(word).unwrap_or(false);
        index = index.saturating_add(if takes_value { 2 } else { 1 });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    fn check(text: &str, client: usize) -> Result<Option<usize>, String> {
        check_step(&words(text), client)
    }

    #[test]
    fn the_pass_through_part_starts_after_the_exe_target_or_a_double_dash() {
        assert_eq!(check("exe cache get", usize::MAX), Ok(Some(2)));
        assert_eq!(
            check("--no-build --no-cache exe @mathlib/lint-style", usize::MAX),
            Ok(Some(4))
        );
        assert_eq!(check("--no-build lint -- --no-build", usize::MAX), Ok(Some(3)));
        assert_eq!(check("-K a=b exec tool -d x", usize::MAX), Ok(Some(4)));
        assert_eq!(check("env sh -c true", usize::MAX), Ok(Some(2)));
        assert_eq!(check("run script -d x", usize::MAX), Ok(Some(2)));
        assert_eq!(check("build Proofs", usize::MAX), Ok(None));
        // A value is not the command: `--only` takes `exe`, so `build` is the command.
        assert_eq!(check("--only exe build x", usize::MAX), Ok(None));
    }

    #[test]
    fn options_that_move_lake_are_refused_in_lakes_part_only() {
        for step in [
            "build -d /x",
            "-d/x build",
            "build -d=/x",
            "build --dir /x",
            "build --dir=/x",
            "--file=x.lean build",
            "-f x build",
        ] {
            assert!(check(step, usize::MAX).is_err(), "{step}");
        }
        assert!(check("--only exe build x -d y", usize::MAX).is_err());
        assert_eq!(check("build -- -d x", usize::MAX), Ok(Some(2)));
        assert_eq!(check("exe tool -d x --file y", usize::MAX), Ok(Some(2)));
        // `-o` takes `-d` as its value, so Lake reads no `-d`.
        assert_eq!(check("build -o -d", usize::MAX), Ok(None));
    }

    #[test]
    fn grouped_short_options_are_refused() {
        // Lake reads `-R-d x` as `-R`, then `-d x`.
        assert!(check("build -R-d x", usize::MAX).is_err());
        assert!(check("-Rexe build", usize::MAX).is_err());
        assert_eq!(check("build -oout.json", usize::MAX), Ok(None));
        assert_eq!(check("build -K=a=b", usize::MAX), Ok(None));
    }

    #[test]
    fn a_step_cannot_end_in_an_option_that_takes_a_value() {
        assert!(check("build -o", usize::MAX).is_err());
        assert!(check("build --packages", usize::MAX).is_err());
        assert_eq!(check("build --packages=p.json", usize::MAX), Ok(None));
        // After the boundary the tool reads its own options.
        assert_eq!(check("exe tool --workers", usize::MAX), Ok(Some(2)));
    }

    #[test]
    fn client_words_that_start_with_a_dash_fit_only_in_the_pass_through_part() {
        assert!(check("build Proofs --wfail", 1).is_err());
        assert!(check("build -- x", 1).is_err());
        assert_eq!(check("exe tool --check A.lean", 2), Ok(Some(2)));
        assert_eq!(check("lint -- --no-build", 3), Ok(Some(2)));
        // A client word can name the exe when the recipe leaves it open.
        assert_eq!(check("exe cache -d x", 1), Ok(Some(2)));
        assert!(check("build", 0).is_ok());
    }

    #[test]
    fn empty_and_nul_words_are_refused() {
        assert!(check_step(&["build".to_owned(), String::new()], usize::MAX).is_err());
        assert!(check_step(&["build".to_owned(), "a\0b".to_owned()], usize::MAX).is_err());
    }

    #[test]
    fn a_step_needs_a_command() {
        assert!(has_command(&words("--no-build exe tool")));
        assert!(has_command(&words("clean Proofs")));
        assert!(!has_command(&words("--no-build -K a=b")));
        assert!(!has_command(&words("-- build")));
    }
}
