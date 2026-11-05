// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::http::byte_range::ByteRange;

#[test]
fn one_range_parses() {
    let from = |first, last| ByteRange::From { first, last };
    assert_eq!(ByteRange::parse("bytes=0-99"), Some(from(0, Some(99))));
    assert_eq!(ByteRange::parse("bytes=5-5"), Some(from(5, Some(5))));
    assert_eq!(ByteRange::parse("bytes=100-"), Some(from(100, None)));
    assert_eq!(ByteRange::parse("bytes=-20"), Some(ByteRange::Suffix(20)));
    assert_eq!(ByteRange::parse("BYTES=1-2"), Some(from(1, Some(2))));
    assert_eq!(ByteRange::parse(" bytes=1-2 "), Some(from(1, Some(2))));
}

/// What it cannot read as exactly one range, the route ignores and serves whole.
#[test]
fn anything_else_does_not_parse() {
    for value in [
        "",
        "bytes",
        "bytes=",
        "bytes=-",
        "bytes=5",
        "bytes=9-3",
        "bytes=a-b",
        "bytes=+1-2",
        "bytes=0-1,4-5",
        "items=0-1",
        "bytes=99999999999999999999999-",
    ] {
        assert_eq!(ByteRange::parse(value), None, "{value}");
    }
}

#[test]
fn a_range_resolves_within_the_content() {
    let from = |first, last| ByteRange::From { first, last };
    assert_eq!(from(0, Some(9)).resolve(100), Some(0..10));
    assert_eq!(from(90, None).resolve(100), Some(90..100));
    // The end clamps to the content.
    assert_eq!(from(90, Some(5000)).resolve(100), Some(90..100));
    assert_eq!(from(0, Some(u64::MAX)).resolve(100), Some(0..100));
    assert_eq!(ByteRange::Suffix(10).resolve(100), Some(90..100));
    assert_eq!(ByteRange::Suffix(500).resolve(100), Some(0..100));
}

#[test]
fn a_range_past_the_content_names_nothing() {
    let from = |first, last| ByteRange::From { first, last };
    assert_eq!(from(100, None).resolve(100), None);
    assert_eq!(from(500, Some(600)).resolve(100), None);
    assert_eq!(ByteRange::Suffix(0).resolve(100), None);
    assert_eq!(ByteRange::Suffix(5).resolve(0), None);
}
