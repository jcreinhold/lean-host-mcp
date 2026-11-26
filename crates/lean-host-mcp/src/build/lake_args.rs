//! The one rule the server applies to the words a client adds to a Lake step.
//!
//! A client names targets, or the arguments a recipe appends. Lake reads a
//! word that starts with `-` as one of its own options wherever it reads
//! options, and an option such as `-d` or `--dir` would run Lake in another
//! project under this project's lock. So a client word may not start with `-`,
//! be empty, or contain NUL. Recipe words come from the project's tracked
//! config, and Lake reports its own errors in them.

/// Check the words a client adds to a step.
///
/// # Errors
/// Names the first word the server refuses.
pub(super) fn check_client_words(words: &[String]) -> Result<(), String> {
    for word in words {
        if word.is_empty() {
            return Err("an argument is empty".to_owned());
        }
        if word.contains('\0') {
            return Err(format!("argument `{}` contains NUL", word.escape_default()));
        }
        if word.starts_with('-') {
            return Err(format!(
                "argument `{word}` starts with `-`; Lake could read it as one of its own options"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_client_words;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn targets_modules_and_files_pass() {
        assert!(check_client_words(&words("Proofs Proofs.A:olean @mathlib/+Mathlib:olean Proofs/A.lean")).is_ok());
        assert!(check_client_words(&[]).is_ok());
    }

    #[test]
    fn dash_empty_and_nul_words_are_refused() {
        for word in ["--wfail", "-d", "--dir=/x", "--", "-"] {
            assert!(check_client_words(&[word.to_owned()]).is_err(), "{word}");
        }
        assert!(check_client_words(&[String::new()]).is_err());
        assert!(check_client_words(&["a\0b".to_owned()]).is_err());
    }
}
