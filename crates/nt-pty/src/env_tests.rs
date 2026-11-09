use super::*;

fn pairs(items: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    items
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

fn build(inherited: &[(&str, &str)], extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    child_env(pairs(inherited).into_iter(), &pairs(extra))
}

/// All values bound to `key`, in order.
fn values<'a>(env: &'a [(OsString, OsString)], key: &str) -> Vec<&'a OsString> {
    env.iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v)
        .collect()
}

fn single(env: &[(OsString, OsString)], key: &str) -> Option<OsString> {
    let found = values(env, key);
    assert!(found.len() <= 1, "{key} appears {} times", found.len());
    found.first().map(|v| (*v).clone())
}

#[test]
fn parent_terminal_variables_are_scrubbed() {
    let env = build(
        &[
            ("WEZTERM_PANE", "1"),
            ("ALACRITTY_LOG", "x"),
            ("COLUMNS", "80"),
        ],
        &[],
    );
    for key in ["WEZTERM_PANE", "ALACRITTY_LOG", "COLUMNS"] {
        assert!(
            values(&env, key).is_empty(),
            "{key} leaked into the child env"
        );
    }
}

#[test]
fn unrelated_inherited_variables_pass_through() {
    let env = build(
        &[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("WEZTERM_PANE", "1"),
        ],
        &[],
    );
    assert_eq!(single(&env, "PATH"), Some("/usr/bin".into()));
    assert_eq!(single(&env, "HOME"), Some("/home/u".into()));
}

#[test]
fn identity_variables_are_set() {
    let env = build(&[], &[]);
    assert!(single(&env, "COLORTERM").is_some_and(|v| !v.is_empty()));
    assert!(single(&env, "TERM_PROGRAM").is_some_and(|v| !v.is_empty()));
    assert!(single(&env, "TERM_PROGRAM_VERSION").is_some_and(|v| !v.is_empty()));
}

#[test]
fn term_defaults_to_xterm_256color_without_extra_term() {
    let env = build(&[], &[("NEWTERM_PANE_ID", "42")]);
    assert_eq!(single(&env, "TERM"), Some("xterm-256color".into()));
}

#[test]
fn inherited_term_is_replaced_by_default() {
    // The launching terminal's TERM describes that terminal, not ours.
    let env = build(&[("TERM", "alacritty")], &[]);
    assert_eq!(single(&env, "TERM"), Some("xterm-256color".into()));
}

#[test]
fn extra_term_and_terminfo_dirs_pass_through_unchanged() {
    let env = build(
        &[],
        &[
            ("TERM", "newterm"),
            ("TERMINFO_DIRS", "/opt/ti:/usr/share/terminfo"),
        ],
    );
    assert_eq!(single(&env, "TERM"), Some("newterm".into()));
    assert_eq!(
        single(&env, "TERMINFO_DIRS"),
        Some("/opt/ti:/usr/share/terminfo".into())
    );
}

#[test]
fn extra_overrides_inherited_values() {
    let env = build(&[("FOO", "inherited"), ("PATH", "/a")], &[("FOO", "extra")]);
    assert_eq!(single(&env, "FOO"), Some("extra".into()));
    assert_eq!(single(&env, "PATH"), Some("/a".into()));
}

#[test]
fn extra_overrides_our_defaults() {
    let env = build(&[], &[("COLORTERM", "custom"), ("TERM_PROGRAM", "other")]);
    assert_eq!(single(&env, "COLORTERM"), Some("custom".into()));
    assert_eq!(single(&env, "TERM_PROGRAM"), Some("other".into()));
}

#[test]
fn extra_wins_even_over_scrubbed_names() {
    let env = build(&[("COLUMNS", "80")], &[("COLUMNS", "100")]);
    assert_eq!(single(&env, "COLUMNS"), Some("100".into()));
}

#[test]
fn no_duplicate_keys_in_output() {
    let env = build(
        &[("A", "1"), ("TERM", "x"), ("COLORTERM", "y")],
        &[("A", "2"), ("TERM", "z"), ("COLORTERM", "w")],
    );
    for (key, _) in &env {
        assert_eq!(
            values(&env, key.to_str().unwrap()).len(),
            1,
            "duplicate {key:?}"
        );
    }
}
