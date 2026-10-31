// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;

use clap::Parser;
use lore_client::cli::IDENTITY_VAR;
use lore_client::cli::LoreCli;
use lore_client::cli::LoreCliError;
use lore_client::cli::resolve_credential;

fn env(value: &str) -> Option<OsString> {
    Some(OsString::from(value))
}

fn resolve(flag: Option<&str>, from_env: Option<OsString>) -> Option<String> {
    resolve_credential(flag, from_env, IDENTITY_VAR).expect("resolves")
}

/// The flag outranks the variable, which stands in only for a flag that was
/// not given.
#[test]
fn the_flag_outranks_the_variable() {
    assert_eq!(
        resolve(Some("flag"), env("variable")),
        Some("flag".to_string())
    );
    assert_eq!(resolve(None, env("variable")), Some("variable".to_string()));
    assert_eq!(resolve(Some("flag"), None), Some("flag".to_string()));
    assert_eq!(resolve(None, None), None);
}

/// An empty flag, as a script passes for an unset shell variable, and a blank
/// variable both count as absent.
#[test]
fn empty_values_count_as_absent() {
    assert_eq!(
        resolve(Some(""), env("variable")),
        Some("variable".to_string())
    );
    assert_eq!(resolve(Some(""), None), None);
    assert_eq!(resolve(None, env("")), None);
    assert_eq!(resolve(None, env("  ")), None);
}

/// A value is passed on as given; only a blank value reads as unset.
#[test]
fn the_variable_is_not_trimmed() {
    assert_eq!(resolve(None, env("person ")), Some("person ".to_string()));
}

#[cfg(unix)]
#[test]
fn a_variable_that_is_not_unicode_is_an_error_naming_it() {
    use std::os::unix::ffi::OsStringExt;
    let invalid = OsString::from_vec(vec![0x66, 0xff]);
    assert_eq!(
        resolve_credential(None, Some(invalid), IDENTITY_VAR),
        Err(LoreCliError::NotUnicode(IDENTITY_VAR))
    );
    let message = LoreCliError::NotUnicode(IDENTITY_VAR).to_string();
    assert!(message.contains("LORE_IDENTITY"), "{message}");
}

#[test]
fn the_variables_are_named_in_the_help() {
    use clap::CommandFactory;
    let help = LoreCli::command().render_help().to_string();
    assert!(help.contains("LORE_ACCESS_TOKEN"), "{help}");
    assert!(help.contains("LORE_IDENTITY"), "{help}");
    assert!(LoreCli::try_parse_from(["lore", "status"]).is_ok());
}
