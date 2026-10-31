// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Resolves the access token, identity token and identity through the real
//! `LORE_ACCESS_TOKEN`, `LORE_IDENTITY_TOKEN` and `LORE_IDENTITY` variables, which
//! are process-wide state, so this runs in a process of its own.
use clap::Parser;
use lore::interface::LoreGlobalArgs;
use lore_client::cli::LoreCli;
use lore_client::cli::lore_globals_from_args;

fn globals(args: &[&str]) -> LoreGlobalArgs {
    let cli = LoreCli::try_parse_from(args).expect("the arguments must parse");
    lore_globals_from_args(&cli).expect("the globals must resolve")
}

/// An opaque token, such as a placeholder a trusted transport resolves.
const OPAQUE: &str = "opaque-placeholder";

/// Each variable stands in for its absent flag, the flag overrides it, and the
/// opaque-token check applies to the result as it does to the flags.
#[test]
fn the_variables_stand_in_for_absent_flags() {
    // safety: the only test in this binary, so nothing else reads the environment
    unsafe {
        std::env::remove_var("LORE_ACCESS_TOKEN");
        std::env::remove_var("LORE_IDENTITY");
        std::env::remove_var("LORE_IDENTITY_TOKEN");
    }
    let unset = globals(&["lore", "status"]);
    assert_eq!(unset.access_token(), "", "nothing set is no token");
    assert_eq!(unset.identity(), None, "nothing set is no identity");

    unsafe {
        std::env::set_var("LORE_ACCESS_TOKEN", OPAQUE);
        std::env::set_var("LORE_IDENTITY", "https://issuer.example#person");
    }
    let mut from_env = globals(&["lore", "status"]);
    assert_eq!(
        from_env.access_token(),
        OPAQUE,
        "the variable sets the token"
    );
    assert_eq!(
        from_env.identity(),
        Some("https://issuer.example#person"),
        "the variable sets the identity"
    );
    assert!(
        from_env.validate().is_ok(),
        "an opaque token from the environment is valid with an identity from it"
    );

    let flags = globals(&[
        "lore",
        "status",
        "--access-token",
        "flag-token",
        "--identity",
        "flag-identity",
    ]);
    assert_eq!(
        flags.access_token(),
        "flag-token",
        "the flag outranks the variable"
    );
    assert_eq!(
        flags.identity(),
        Some("flag-identity"),
        "the flag outranks the variable"
    );

    let with_identity_token = globals(&["lore", "status", "--identity-token", "token"]);
    assert_eq!(
        with_identity_token.identity(),
        None,
        "an identity token names its own identity, so the variable does not apply"
    );

    let with_explicit_identity = globals(&["lore", "status", "--identity", "flag-identity"]);
    assert_eq!(
        with_explicit_identity.access_token(),
        OPAQUE,
        "an explicit identity keeps the opaque token from the variable"
    );

    unsafe { std::env::set_var("LORE_IDENTITY_TOKEN", "identity-token") };
    let with_identity_token_variable = globals(&["lore", "status"]);
    assert_eq!(
        with_identity_token_variable.identity_token(),
        "identity-token",
        "the variable sets the identity token"
    );
    assert_eq!(
        with_identity_token_variable.identity(),
        None,
        "an identity token from its variable names its own identity too"
    );
    let identity_outranks = globals(&["lore", "status", "--identity", "flag-identity"]);
    assert_eq!(
        identity_outranks.identity_token(),
        "",
        "an explicit identity outranks the identity token variable"
    );
    unsafe { std::env::remove_var("LORE_IDENTITY_TOKEN") };

    unsafe { std::env::set_var("LORE_IDENTITY", "") };
    let mut without_identity = globals(&["lore", "status"]);
    assert_eq!(
        without_identity.identity(),
        None,
        "a blank variable is unset"
    );
    let refused = without_identity
        .validate()
        .expect_err("an opaque token still requires an identity");
    assert!(
        refused.to_string().contains("explicit identity"),
        "{refused}"
    );
}
