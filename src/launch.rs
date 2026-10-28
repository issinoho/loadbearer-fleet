//! When a CPU generation launched, from its model string — so a run from a
//! loadbearer older than 1.6.0, which recorded no `machine.age`, still gets a
//! build-date floor.
//!
//! **A copy of loadbearer's table** (`src/age.rs` there), not a dependency on
//! it: loadbearer's Rust API is outside its stability contract (see
//! `schema.rs`), so the two projects share the document and nothing else. The
//! cost is that a correction made there has to be made here too. Where a run
//! *did* record its age, that always wins over this — see
//! `analytics::AgeSource`.
//!
//! The launch is the earliest in the CPU's generation, so "built this month or
//! later" stays true at the cost of a few months' precision. A CPU it doesn't
//! know gets `None`, never a guess.

/// When a CPU generation first shipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// "12th-gen Intel Core (mobile)", "Apple M3 Pro", …
    pub family: String,
    pub year: u16,
    pub month: u8,
}

impl Launch {
    /// `YYYY-MM`, the form loadbearer records `cpu_launched` in.
    pub fn year_month(&self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }
}

/// Intel Core i3/i5/i7/i9 by generation: `(desktop, mobile)` first launch.
/// The two differ by up to half a year, and the model suffix says which.
const INTEL_CORE_GENS: [((u16, u8), (u16, u8)); 14] = [
    ((2008, 11), (2009, 9)), // 1st: Nehalem / Westmere
    ((2011, 1), (2011, 1)),  // 2nd: Sandy Bridge
    ((2012, 4), (2012, 4)),  // 3rd: Ivy Bridge
    ((2013, 6), (2013, 6)),  // 4th: Haswell
    ((2015, 6), (2015, 1)),  // 5th: Broadwell
    ((2015, 8), (2015, 9)),  // 6th: Skylake
    ((2017, 1), (2016, 8)),  // 7th: Kaby Lake
    ((2017, 10), (2017, 8)), // 8th: Coffee Lake / Kaby Lake R
    ((2018, 10), (2019, 4)), // 9th: Coffee Lake R
    ((2020, 4), (2019, 8)),  // 10th: Comet Lake / Ice Lake
    ((2021, 3), (2020, 9)),  // 11th: Rocket Lake / Tiger Lake
    ((2021, 11), (2022, 1)), // 12th: Alder Lake
    ((2022, 10), (2023, 1)), // 13th: Raptor Lake
    ((2023, 10), (2024, 1)), // 14th: Raptor Lake Refresh
];

/// Map a CPU brand string — as the OS reports it, or as `models` writes it —
/// to when its generation launched. `None` for anything not in the table:
/// Xeons, older Atoms and most server parts aren't, because their numbering
/// doesn't encode the generation reliably enough to give a true floor.
pub fn cpu_launch(model: &str) -> Option<Launch> {
    let t = tokens(model);
    apple(&t)
        .or_else(|| intel_core_ultra(&t))
        .or_else(|| intel_core_i(&t))
        .or_else(|| intel_core_series(&t))
        .or_else(|| intel_n(&t))
        .or_else(|| intel_atom_class(&t))
        .or_else(|| amd_ryzen_ai(&t))
        .or_else(|| amd_threadripper(&t))
        .or_else(|| amd_ryzen(&t))
        .or_else(|| amd_epyc(&t))
        .or_else(|| snapdragon_x(&t))
}

fn launch(family: impl Into<String>, (year, month): (u16, u8)) -> Option<Launch> {
    Some(Launch {
        family: family.into(),
        year,
        month,
    })
}

/// Lower-cased words with the ®/™ marks gone, so "Intel(R) Core(TM) i7-1265U"
/// and "Intel Core i7-1265U" read the same.
fn tokens(model: &str) -> Vec<String> {
    let mut s = model.to_lowercase();
    for mark in ["(r)", "(tm)", "®", "™"] {
        s = s.replace(mark, " ");
    }
    s.split_whitespace().map(str::to_string).collect()
}

/// The words after the first `word`.
fn after<'a>(t: &'a [String], word: &str) -> Option<&'a [String]> {
    let i = t.iter().position(|x| x == word)?;
    Some(&t[i + 1..])
}

/// "1265u" → ("1265", "u").
fn split_digits(tok: &str) -> (&str, &str) {
    let n = tok.bytes().take_while(u8::is_ascii_digit).count();
    tok.split_at(n)
}

fn ordinal(n: usize) -> String {
    match n {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        _ => format!("{n}th"),
    }
}

/// Apple M-series, by Mac launch.
fn apple(t: &[String]) -> Option<Launch> {
    let rest = after(t, "apple")?;
    let n: u8 = rest.first()?.strip_prefix('m')?.parse().ok()?;
    let tier = rest.get(1).map(String::as_str).unwrap_or("");
    let date = match (n, tier) {
        (1, "pro" | "max") => (2021, 10),
        (1, "ultra") => (2022, 3),
        (1, _) => (2020, 11),
        (2, "pro" | "max") => (2023, 1),
        (2, "ultra") => (2023, 6),
        (2, _) => (2022, 6),
        (3, "ultra") => (2025, 3),
        (3, _) => (2023, 10),
        (4, _) => (2024, 10),
        (5, _) => (2025, 10),
        _ => return None,
    };
    let tier = match tier {
        "pro" => " Pro",
        "max" => " Max",
        "ultra" => " Ultra",
        _ => "",
    };
    launch(format!("Apple M{n}{tier}"), date)
}

/// "Core Ultra 7 155H": the series is the model number's first digit.
fn intel_core_ultra(t: &[String]) -> Option<Launch> {
    let rest = after(t, "ultra")?;
    if !t.iter().any(|x| x == "core") {
        return None;
    }
    let (digits, _) = split_digits(rest.get(1)?);
    if digits.len() != 3 {
        return None;
    }
    let (series, date) = match &digits[..1] {
        "1" => (1, (2023, 12)), // Meteor Lake
        "2" => (2, (2024, 9)),  // Lunar Lake, then Arrow Lake
        "3" => (3, (2026, 1)),  // Panther Lake
        _ => return None,
    };
    launch(format!("Intel Core Ultra Series {series}"), date)
}

/// "i7-1265U", "i5-8350U", "i9-13900K", "i7-920", "i3-N305".
fn intel_core_i(t: &[String]) -> Option<Launch> {
    let tok = t.iter().find(|x| {
        let b = x.as_bytes();
        b.len() > 3 && b[0] == b'i' && b[1].is_ascii_digit() && b[2] == b'-'
    })?;
    let model = &tok[3..];
    if model.starts_with('n') {
        // i3-N300 / N305: Alder Lake-N, same as the plain N-series.
        return launch("Intel Core i3 N-series", (2023, 1));
    }
    let (digits, suffix) = split_digits(model);
    let generation: usize = match digits.len() {
        3 => 1,
        // 10th–13th-gen mobile parts are four digits starting "1x": i7-1265U.
        4 if digits.starts_with('1') => digits[..2].parse().ok()?,
        4 => digits[..1].parse().ok()?,
        5 => digits[..2].parse().ok()?,
        _ => return None,
    };
    let (desktop, mobile) = *INTEL_CORE_GENS.get(generation.checked_sub(1)?)?;
    let is_mobile = suffix.starts_with(['u', 'y', 'h', 'p', 'g', 'm', 'q']);
    let (date, kind) = if is_mobile {
        (mobile, "mobile")
    } else {
        (desktop, "desktop")
    };
    launch(
        format!("{}-gen Intel Core ({kind})", ordinal(generation)),
        date,
    )
}

/// The non-Ultra "Core 7 150U" branding: series is the first digit.
fn intel_core_series(t: &[String]) -> Option<Launch> {
    let rest = after(t, "core")?;
    if !matches!(rest.first()?.as_str(), "3" | "5" | "7" | "9") {
        return None;
    }
    let (digits, _) = split_digits(rest.get(1)?);
    if digits.len() != 3 {
        return None;
    }
    let (series, date) = match &digits[..1] {
        "1" => (1, (2024, 1)),
        "2" => (2, (2025, 1)),
        _ => return None,
    };
    launch(format!("Intel Core Series {series}"), date)
}

/// "Intel N95", "Intel N100": Alder Lake-N, and its Twin Lake refresh.
fn intel_n(t: &[String]) -> Option<Launch> {
    if !t.iter().any(|x| x == "intel") {
        return None;
    }
    t.iter().find_map(|x| {
        let (digits, rest) = split_digits(x.strip_prefix('n')?);
        if !(2..=3).contains(&digits.len()) || !rest.is_empty() {
            return None;
        }
        match digits {
            "150" | "250" | "350" | "355" => launch("Intel N-series (Twin Lake)", (2025, 1)),
            _ => launch("Intel N-series (Alder Lake-N)", (2023, 1)),
        }
    })
}

/// Atom-class Celeron / Pentium (J4005, N5105, N2840). The numbering only
/// separates the eras coarsely, so the floor is the earliest of each.
fn intel_atom_class(t: &[String]) -> Option<Launch> {
    if !t.iter().any(|x| x == "celeron" || x == "pentium") {
        return None;
    }
    t.iter().find_map(|x| {
        let (digits, rest) = split_digits(x.strip_prefix(['j', 'n'])?);
        if digits.len() != 4 || !rest.is_empty() {
            return None;
        }
        match &digits[..1] {
            "1" | "2" | "3" => launch("Intel Celeron/Pentium (Bay Trail or later)", (2013, 9)),
            "4" | "5" | "6" => launch("Intel Celeron/Pentium (Gemini Lake or later)", (2017, 12)),
            _ => None,
        }
    })
}

/// "Ryzen AI 9 HX 370", "Ryzen AI Max+ 395".
fn amd_ryzen_ai(t: &[String]) -> Option<Launch> {
    let rest = after(t, "ryzen")?;
    if rest.first()? != "ai" {
        return None;
    }
    if rest.get(1).is_some_and(|x| x.starts_with("max")) {
        launch("AMD Ryzen AI Max", (2025, 1))
    } else {
        launch("AMD Ryzen AI", (2024, 7))
    }
}

/// "Threadripper 3970X", "Threadripper PRO 5995WX".
fn amd_threadripper(t: &[String]) -> Option<Launch> {
    let rest = after(t, "threadripper")?;
    let tok = rest.iter().find(|x| *x != "pro")?;
    let (digits, _) = split_digits(tok);
    if digits.len() != 4 {
        return None;
    }
    let date = match &digits[..1] {
        "1" => (2017, 8),
        "2" => (2018, 8),
        "3" => (2019, 11),
        "5" => (2022, 3),
        "7" => (2023, 10),
        "9" => (2024, 7),
        _ => return None,
    };
    launch(format!("AMD Ryzen Threadripper {}000", &digits[..1]), date)
}

/// "Ryzen 7 5800X", "Ryzen 7 PRO 5850U": the series is the first digit,
/// earliest of desktop and mobile.
fn amd_ryzen(t: &[String]) -> Option<Launch> {
    let rest = after(t, "ryzen")?;
    if !matches!(rest.first()?.as_str(), "3" | "5" | "7" | "9") {
        return None;
    }
    let tok = rest[1..].iter().find(|x| *x != "pro")?;
    let (digits, _) = split_digits(tok);
    if digits.len() != 4 {
        return None;
    }
    let date = match &digits[..1] {
        "1" => (2017, 3),
        "2" => (2017, 10),
        "3" => (2019, 1),
        "4" => (2020, 3),
        "5" => (2020, 11),
        "6" => (2022, 1),
        "7" => (2022, 9),
        "8" => (2023, 12),
        "9" => (2024, 8),
        _ => return None,
    };
    launch(format!("AMD Ryzen {}000 series", &digits[..1]), date)
}

/// "EPYC 7763": the generation is the *last* digit.
fn amd_epyc(t: &[String]) -> Option<Launch> {
    let (digits, _) = split_digits(after(t, "epyc")?.first()?);
    if digits.len() != 4 {
        return None;
    }
    let (name, date) = match &digits[3..] {
        "1" => ("Naples", (2017, 6)),
        "2" => ("Rome", (2019, 8)),
        "3" => ("Milan", (2021, 3)),
        "4" => ("Genoa", (2022, 11)),
        "5" => ("Turin", (2024, 10)),
        _ => return None,
    };
    launch(format!("AMD EPYC ({name})"), date)
}

/// First-generation Snapdragon X Elite / Plus (X1E…, X1P…).
fn snapdragon_x(t: &[String]) -> Option<Launch> {
    if t.iter()
        .any(|x| x.starts_with("x1e") || x.starts_with("x1p"))
    {
        launch("Qualcomm Snapdragon X", (2024, 6))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(model: &str) -> Option<(String, u16, u8)> {
        cpu_launch(model).map(|l| (l.family, l.year, l.month))
    }

    #[test]
    fn os_brand_strings_resolve() {
        for (brand, family, y, m) in [
            (
                "12th Gen Intel(R) Core(TM) i7-1265U",
                "12th-gen Intel Core (mobile)",
                2022,
                1,
            ),
            (
                "Intel(R) Core(TM) i5-8350U CPU @ 1.70GHz",
                "8th-gen Intel Core (mobile)",
                2017,
                8,
            ),
            (
                "Intel(R) Core(TM) i7-10510U CPU @ 1.80GHz",
                "10th-gen Intel Core (mobile)",
                2019,
                8,
            ),
            (
                "13th Gen Intel(R) Core(TM) i9-13900K",
                "13th-gen Intel Core (desktop)",
                2022,
                10,
            ),
            (
                "Intel(R) Core(TM) i7-1065G7 CPU @ 1.30GHz",
                "10th-gen Intel Core (mobile)",
                2019,
                8,
            ),
            (
                "Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz",
                "8th-gen Intel Core (desktop)",
                2017,
                10,
            ),
            ("Intel(R) Core(TM) i7 CPU 920 @ 2.67GHz", "", 0, 0),
            (
                "Intel(R) Core(TM) Ultra 7 155H",
                "Intel Core Ultra Series 1",
                2023,
                12,
            ),
            ("Intel(R) Core(TM) 7 150U", "Intel Core Series 1", 2024, 1),
            (
                "Intel(R) Core(TM) i3-N305",
                "Intel Core i3 N-series",
                2023,
                1,
            ),
            ("Intel(R) N100", "Intel N-series (Alder Lake-N)", 2023, 1),
            (
                "Intel(R) Celeron(R) J4005 CPU @ 2.00GHz",
                "Intel Celeron/Pentium (Gemini Lake or later)",
                2017,
                12,
            ),
            (
                "AMD Ryzen 7 5800X 8-Core Processor",
                "AMD Ryzen 5000 series",
                2020,
                11,
            ),
            (
                "AMD Ryzen 7 PRO 5850U with Radeon Graphics",
                "AMD Ryzen 5000 series",
                2020,
                11,
            ),
            (
                "AMD Ryzen AI 9 HX 370 w/ Radeon 890M",
                "AMD Ryzen AI",
                2024,
                7,
            ),
            (
                "AMD RYZEN AI MAX+ 395 w/ Radeon 8060S",
                "AMD Ryzen AI Max",
                2025,
                1,
            ),
            (
                "AMD Ryzen Threadripper PRO 5995WX 64-Cores",
                "AMD Ryzen Threadripper 5000",
                2022,
                3,
            ),
            (
                "AMD EPYC 7763 64-Core Processor",
                "AMD EPYC (Milan)",
                2021,
                3,
            ),
            (
                "Snapdragon(R) X Elite - X1E78100 - Qualcomm(R) Oryon(TM) CPU",
                "Qualcomm Snapdragon X",
                2024,
                6,
            ),
            ("Apple M1 Max", "Apple M1 Max", 2021, 10),
        ] {
            let want = (!family.is_empty()).then(|| (family.to_string(), y, m));
            assert_eq!(at(brand), want, "{brand}");
        }
    }

    /// The CPU on the dashboard's real 1.5.2-era runs.
    #[test]
    fn the_estates_current_cpus_resolve() {
        let l = cpu_launch("Intel(R) Core(TM) Ultra 7 165H").unwrap();
        assert_eq!(l.family, "Intel Core Ultra Series 1");
        assert_eq!(l.year_month(), "2023-12");
    }

    #[test]
    fn unknown_parts_get_no_date_rather_than_a_guess() {
        for brand in [
            "Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz",
            "Intel(R) Core(TM)2 Duo CPU E8400 @ 3.00GHz",
            "Intel(R) Celeron(R) G5900",
            "QEMU Virtual CPU version 2.5+",
            "unknown",
            "",
        ] {
            assert_eq!(cpu_launch(brand), None, "{brand}");
        }
    }
}
