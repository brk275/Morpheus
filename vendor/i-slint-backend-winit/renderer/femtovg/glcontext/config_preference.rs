// Copyright Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

// Transparency is the primary preference; samples only break ties within the
// same known-transparency class. Unknown support remains a usable fallback.
pub(super) fn prefer_gl_config(
    candidate_transparency: Option<bool>,
    current_transparency: Option<bool>,
    fewer_samples: impl FnOnce() -> bool,
) -> bool {
    match (
        candidate_transparency.unwrap_or(false),
        current_transparency.unwrap_or(false),
    ) {
        (true, false) => true,
        (false, true) => false,
        _ => fewer_samples(),
    }
}

#[cfg(test)]
mod tests {
    use super::prefer_gl_config;

    #[test]
    fn known_transparency_outranks_samples_in_both_native_orders() {
        for fallback in [Some(false), None] {
            assert!(prefer_gl_config(Some(true), fallback, || {
                panic!("samples must not override known transparency")
            }));
            assert!(!prefer_gl_config(fallback, Some(true), || {
                panic!("samples must not discard known transparency")
            }));
            for candidates in [
                [(Some(true), 4_u8), (fallback, 0_u8)],
                [(fallback, 0_u8), (Some(true), 4_u8)],
            ] {
                let selected = candidates.into_iter().reduce(|current, candidate| {
                    if prefer_gl_config(candidate.0, current.0, || candidate.1 < current.1) {
                        candidate
                    } else {
                        current
                    }
                });
                assert_eq!(selected, Some((Some(true), 4)));
            }
        }
    }

    #[test]
    fn unknown_opaque_fallbacks_and_sample_ties_preserve_order() {
        for (candidate, current) in [
            (None, None),
            (None, Some(false)),
            (Some(false), None),
            (Some(false), Some(false)),
            (Some(true), Some(true)),
        ] {
            for (candidate_samples, current_samples) in [(0, 4), (4, 0), (4, 4), (0, 0), (255, 0)] {
                let calls = std::cell::Cell::new(0);
                let preferred = prefer_gl_config(candidate, current, || {
                    calls.set(calls.get() + 1);
                    candidate_samples < current_samples
                });
                assert_eq!(preferred, candidate_samples < current_samples);
                assert_eq!(calls.get(), 1);
            }
        }
    }
}
