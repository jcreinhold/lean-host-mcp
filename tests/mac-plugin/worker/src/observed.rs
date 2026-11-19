pub(super) fn match_order(actual: Option<&[String]>, expected: &[String]) -> Option<Vec<String>> {
    actual
        .filter(|roots| !roots.is_empty() && *roots == expected)
        .map(<[String]>::to_vec)
}
#[cfg(test)]
mod tests {
    use super::match_order;
    #[test]
    fn source_gate_requires_the_session_order() {
        let expected = vec![String::from("/project"), String::from("/release")];
        assert!(match_order(None, &expected).is_none());
        assert!(match_order(Some(&[]), &expected).is_none());
        assert!(match_order(Some(&[expected[1].clone(), expected[0].clone()]), &expected).is_none());
        assert_eq!(match_order(Some(&expected), &expected), Some(expected));
    }
}
