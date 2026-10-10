//! The screen class of a persona (spec §6.1 step 3, follow-up F19).
//!
//! The engines' generators hold real devices, but not all of them make a
//! coherent persona here. Both engines present a device pixel ratio of 1,
//! so a size that only exists as a scaled one is a contradiction a page can
//! compute: 1536x864 is 1920x1080 at 125 %, and a Mac notebook's own 1728x1117
//! always comes with a ratio of 2. Measured in milestone M6 on the pinned
//! generators (`m6-evidence/experiments/`):
//!
//! - The Firefox generator has about one recorded device per size. For
//!   Windows a third of its draws are 1376x774, one user's custom scaling
//!   that every Camoufox installation draws equally often; another fifth
//!   are scaled sizes; the device at 1680x1050 has its window outside its
//!   own screen. For macOS most draws are a notebook's built-in panel.
//! - Asking the generator for an exact size works for every size it holds:
//!   the document is then that real device's record.
//!
//! Kage therefore draws a class from its own table and sends it as exact
//! bounds. A table lists only sizes that are physical panels, that the
//! pinned generator holds a device for, and that were measured coherent;
//! never 1280x720, the default of headless tools. For macOS that leaves a
//! Mac with an external display, the only kind of Mac a ratio of 1 fits.
//!
//! Draws are independent per persona: a real sample of households clusters
//! on 1920x1080 too. The tables are [`super::TABLES_VERSION`]; a persona
//! keeps the screen it was generated with.

use crate::domain::{ClaimedOs, Engine};
use crate::seed::Seed;

/// A screen size a persona may have, with its share among the personas of
/// its engine and system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScreenClass {
    /// Width in CSS pixels, which are device pixels here.
    pub width: u32,
    /// Height in CSS pixels.
    pub height: u32,
    /// Share of the class, relative to the sum of the table's weights.
    pub weight: u32,
}

const fn class(width: u32, height: u32, weight: u32) -> ScreenClass {
    ScreenClass {
        width,
        height,
        weight,
    }
}

/// Firefox claiming Windows. Left out on purpose: 1376x774, the scaled
/// sizes (1707x960, 1707x1067, 2176x1224, 3072x1728) and 1680x1050.
const FIREFOX_WINDOWS: &[ScreenClass] = &[
    class(1920, 1080, 60),
    class(2560, 1440, 18),
    class(1600, 900, 10),
    class(3440, 1440, 8),
    class(3840, 1080, 4),
];

/// Firefox claiming macOS: external displays only.
const FIREFOX_MACOS: &[ScreenClass] = &[
    class(2560, 1440, 40),
    class(1920, 1080, 30),
    class(3440, 1440, 12),
    class(2560, 1600, 10),
    class(1680, 1050, 8),
];

/// Chrome on Linux. No 4K: software WebGL is slow at that size and
/// screenshots are large.
const CHROMIUM_LINUX: &[ScreenClass] = &[
    class(1920, 1080, 50),
    class(2560, 1440, 20),
    class(1680, 1050, 6),
    class(1366, 768, 6),
    class(1440, 900, 5),
    class(1600, 900, 4),
    class(1920, 1200, 4),
    class(2560, 1600, 3),
    class(1280, 800, 2),
];

/// The screen classes of an engine and claimed system. Empty for a
/// combination that is not in the default mix and was not measured
/// (firefox on Linux, chromium claiming another system): its personas take
/// what the generator draws inside its default bounds.
pub const fn classes(engine: Engine, os: ClaimedOs) -> &'static [ScreenClass] {
    match (engine, os) {
        (Engine::Firefox, ClaimedOs::Windows) => FIREFOX_WINDOWS,
        (Engine::Firefox, ClaimedOs::Macos) => FIREFOX_MACOS,
        (Engine::Chromium, ClaimedOs::Linux) => CHROMIUM_LINUX,
        _ => &[],
    }
}

/// Draws the screen class of a persona from its seed, by weight. `None`
/// when the engine and system have no table.
pub fn draw(engine: Engine, os: ClaimedOs, seed: &Seed) -> Option<ScreenClass> {
    let table = classes(engine, os);
    let total: u64 = table.iter().map(|class| u64::from(class.weight)).sum();
    if total == 0 {
        return None;
    }
    let mut pick = seed.child("screen-class").rng().int_inclusive(1, total);
    table.iter().copied().find(|class| {
        let weight = u64::from(class.weight);
        if pick <= weight {
            return true;
        }
        pick -= weight;
        false
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{ScreenClass, classes, draw};
    use crate::domain::{ClaimedOs, Engine};
    use crate::seed::Seed;

    const TABLES: [(Engine, ClaimedOs); 3] = [
        (Engine::Firefox, ClaimedOs::Windows),
        (Engine::Firefox, ClaimedOs::Macos),
        (Engine::Chromium, ClaimedOs::Linux),
    ];

    /// Sizes that are a larger panel scaled down, or a notebook's own panel
    /// that always comes with a ratio above 1, or a headless default.
    const NEVER: [(u32, u32); 12] = [
        (1376, 774),
        (1536, 864),
        (1707, 960),
        (1707, 1067),
        (2176, 1224),
        (3072, 1728),
        (1728, 1117),
        (1512, 982),
        (1710, 1107),
        (1800, 1169),
        (1280, 720),
        (800, 600),
    ];

    #[test]
    fn the_tables_are_the_ones_that_were_agreed() {
        let sizes = |engine, os| -> Vec<(u32, u32, u32)> {
            classes(engine, os)
                .iter()
                .map(|class| (class.width, class.height, class.weight))
                .collect()
        };
        assert_eq!(
            sizes(Engine::Firefox, ClaimedOs::Windows),
            [
                (1920, 1080, 60),
                (2560, 1440, 18),
                (1600, 900, 10),
                (3440, 1440, 8),
                (3840, 1080, 4)
            ]
        );
        assert_eq!(
            sizes(Engine::Firefox, ClaimedOs::Macos),
            [
                (2560, 1440, 40),
                (1920, 1080, 30),
                (3440, 1440, 12),
                (2560, 1600, 10),
                (1680, 1050, 8)
            ]
        );
        assert_eq!(sizes(Engine::Chromium, ClaimedOs::Linux).len(), 9);
        for (engine, os) in TABLES {
            let table = classes(engine, os);
            assert_eq!(
                table.iter().map(|class| class.weight).sum::<u32>(),
                100,
                "{engine} {os}"
            );
            for class in table {
                assert!(
                    !NEVER.contains(&(class.width, class.height)),
                    "{engine} {os}: {}x{}",
                    class.width,
                    class.height
                );
                // What the sidecar contract accepts as a screen.
                assert!((640..=7680).contains(&class.width));
                assert!((480..=4320).contains(&class.height));
            }
        }
        // The incoherent Windows record is out; no 4K on software WebGL.
        assert!(
            !sizes(Engine::Firefox, ClaimedOs::Windows)
                .iter()
                .any(|(width, height, _)| (*width, *height) == (1680, 1050))
        );
        assert!(
            sizes(Engine::Chromium, ClaimedOs::Linux)
                .iter()
                .all(|(width, height, _)| *width <= 2560 && *height <= 1600)
        );
    }

    #[test]
    fn draws_follow_the_weights() {
        for (engine, os) in TABLES {
            let table = classes(engine, os);
            let mut drawn: BTreeMap<(u32, u32), u32> = BTreeMap::new();
            let runs = 20_000_u32;
            for index in 0..runs {
                let class = draw(engine, os, &Seed::from_u64(u64::from(index))).unwrap();
                *drawn.entry((class.width, class.height)).or_default() += 1;
            }
            for class in table {
                let share = f64::from(drawn[&(class.width, class.height)]) / f64::from(runs);
                let expected = f64::from(class.weight) / 100.0;
                assert!(
                    (share - expected).abs() < 0.015,
                    "{engine} {os} {}x{}: {share:.3} instead of {expected}",
                    class.width,
                    class.height
                );
            }
        }
    }

    /// How many distinct screens twenty personas have, and how many share
    /// the most common one.
    fn variety(screens: &[ScreenClass]) -> (usize, usize) {
        let mut counts: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for class in screens {
            *counts.entry((class.width, class.height)).or_default() += 1;
        }
        (counts.len(), counts.values().max().copied().unwrap_or(0))
    }

    /// The screen half of the acceptance criterion of milestone M6, on
    /// twenty seeds: every screen is from the table, there are at least
    /// three, and none is on more than three quarters of the personas. The
    /// processor half is the generator's and is tested in the sidecar.
    #[test]
    fn twenty_seeds_of_one_kind_have_at_least_three_screens_and_none_dominates() {
        for (engine, os) in TABLES {
            let screens: Vec<ScreenClass> = (0..20_u64)
                .map(|persona| draw(engine, os, &Seed::from_u64(persona)).unwrap())
                .collect();
            for class in &screens {
                assert!(classes(engine, os).contains(class));
            }
            let (distinct, most) = variety(&screens);
            assert!(distinct >= 3, "{engine} {os}: only {distinct} screens");
            assert!(most <= 15, "{engine} {os}: {most} of 20 share one screen");
        }
    }

    /// The same over many pools. The draws are independent, as households
    /// are, so this is a statement about nearly all pools and not about
    /// every one: with 1920x1080 on 60 % of the Windows personas, sixteen
    /// of twenty share it in about one pool in twenty.
    #[test]
    fn nearly_every_pool_of_twenty_has_that_variety() {
        for (engine, os) in TABLES {
            let pools = 2_000_u32;
            let mut three_or_more = 0_u32;
            let mut none_dominates = 0_u32;
            for pool in 0..pools {
                let screens: Vec<ScreenClass> = (0..20_u64)
                    .map(|persona| {
                        let seed =
                            Seed::derive("pool", &[&pool.to_le_bytes(), &persona.to_le_bytes()]);
                        draw(engine, os, &seed).unwrap()
                    })
                    .collect();
                let (distinct, most) = variety(&screens);
                three_or_more += u32::from(distinct >= 3);
                none_dominates += u32::from(most <= 15);
            }
            let share = |count: u32| f64::from(count) / f64::from(pools);
            assert!(
                share(three_or_more) >= 0.98,
                "{engine} {os}: three screens or more in {:.3} of the pools",
                share(three_or_more)
            );
            assert!(
                share(none_dominates) >= 0.92,
                "{engine} {os}: no dominating screen in {:.3} of the pools",
                share(none_dominates)
            );
        }
    }

    #[test]
    fn one_persona_has_one_screen_and_unmeasured_kinds_have_no_table() {
        let seed = Seed::from_u64(42);
        assert_eq!(
            draw(Engine::Firefox, ClaimedOs::Windows, &seed),
            draw(Engine::Firefox, ClaimedOs::Windows, &seed)
        );
        for (engine, os) in [
            (Engine::Firefox, ClaimedOs::Linux),
            (Engine::Chromium, ClaimedOs::Windows),
            (Engine::Chromium, ClaimedOs::Macos),
        ] {
            assert_eq!(classes(engine, os), []);
            assert_eq!(draw(engine, os, &seed), None);
        }
    }
}
