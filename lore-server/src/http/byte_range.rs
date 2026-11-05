// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The single-range subset of HTTP range requests (RFC 9110 §14) that content
//! routes answer. A `Range` this does not understand, several ranges among them,
//! is ignored, as the RFC lets a server do, and the whole content is served.

use std::ops::Range;

/// One `bytes` range as a request names it, before the content's size is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=first-last` or `bytes=first-`.
    From { first: u64, last: Option<u64> },
    /// `bytes=-length`: the last `length` bytes.
    Suffix(u64),
}

fn digits(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

impl ByteRange {
    /// The range a `Range` header's value names, if it names exactly one.
    pub fn parse(value: &str) -> Option<Self> {
        let (unit, spec) = value.trim().split_once('=')?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return None;
        }
        let (first, last) = spec.trim().split_once('-')?;
        match (first, last) {
            ("", suffix) => digits(suffix).map(Self::Suffix),
            (first, "") => digits(first).map(|first| Self::From { first, last: None }),
            (first, last) => {
                let (first, last) = (digits(first)?, digits(last)?);
                (first <= last).then_some(Self::From {
                    first,
                    last: Some(last),
                })
            }
        }
    }

    /// The bytes this names in content of `size` bytes, `None` when it names none.
    pub fn resolve(self, size: u64) -> Option<Range<u64>> {
        match self {
            Self::From { first, last } => (first < size).then(|| {
                let end = last.map_or(size, |last| last.saturating_add(1).min(size));
                first..end
            }),
            Self::Suffix(length) => {
                (length > 0 && size > 0).then(|| size.saturating_sub(length)..size)
            }
        }
    }
}
