//! Head-to-head comparison of indexed runs.
//!
//! The same idea as `loadbearer compare`, over the index instead of over files:
//! for every subtest the runs have in common, each run gets a
//! **direction-adjusted ratio** to the first, and those are rolled up by
//! geometric mean into a figure per component and one overall. The verdict is
//! built from raw metrics, so it does not depend on the baseline or the curve
//! anything was scored with — which is what makes it usable across an estate
//! whose machines were measured over months.
//!
//! Three things it refuses to do, each of which would produce a confident
//! number that means nothing:
//!
//! - **Compare without a direction.** A ratio only has a sign if you know which
//!   way is better, and runs indexed before 0.5.3 have no `direction` recorded
//!   (see `scan --reindex`). Those subtests are dropped and named.
//! - **Compare a peak against a median.** `representative` says which statistic
//!   a value is; 1.5.0 added peak reporting, so a fleet can hold both. Mixing
//!   them measures the statistic, not the machine.
//! - **Roll up across configurations.** A different duration preset or profile
//!   changes what was measured. That is a warning rather than a refusal — it is
//!   sometimes exactly what you want to see — but it is never silent.
//!
//! The schema tag is this project's own: the shape follows
//! `loadbearer.compare/1` closely, but claiming that tag would promise a
//! compatibility we would then have to keep.

use anyhow::{Result, bail};
use rusqlite::Connection;
use serde::Serialize;

use crate::analytics::Comparability;

/// Additive fields keep `/1`; a removal, rename or type change bumps it.
pub const COMPARE_SCHEMA: &str = "loadbearer-fleet.compare/1";

/// How many runs one comparison may hold.
///
/// Not a technical limit — the maths is the same for twenty. It is a reading
/// limit: a table wider than this cannot be read on a laptop, let alone a
/// phone, and a comparison nobody can read is not a comparison.
pub const MAX_RUNS: usize = 4;

/// Below this many shared measurements, the verdict says so before it says
/// anything else.
///
/// A full run has around thirty. Five is the point at which a geometric mean
/// stops being one lucky subtest and starts being a shape — still few, which
/// is why the sentence names the count rather than hiding it.
pub const THIN_COMPARISON: usize = 5;

/// One run in the comparison, as the reader needs to identify it.
#[derive(Debug, Clone, Serialize)]
pub struct RunRef {
    pub run_id: i64,
    pub machine_key: String,
    /// Hostname where there is one, else the machine key.
    pub label: String,
    pub taken_at: String,
    pub cpu_model: String,
    pub comparability: Comparability,
    pub overall_score: Option<f64>,
    pub overall_grade: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubtestComparison {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub direction: String,
    /// Which statistic these values are — `median` or `peak`. The same for
    /// every run, because a mixture is refused.
    pub representative: Option<String>,
    /// Raw value per run, in run order.
    pub values: Vec<f64>,
    /// Direction-adjusted ratio to run 0 (`rel[0] == 1.0`).
    pub rel: Vec<f64>,
    pub best: usize,
    /// `false` for an informational subtest: shown with its own delta, kept
    /// out of the component and overall rollup.
    pub scored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentComparison {
    pub id: String,
    pub label: String,
    /// Whether this component counts toward the overall figure.
    ///
    /// Network and GPU are measured and shown and never graded: they depend on
    /// the host OS, the driver and whatever the network is doing, not on the
    /// silicon. Folding them into a verdict produces a ranking that disagrees
    /// with the scores on the same page — which is what happened the first
    /// time three real machines went through this.
    pub graded: bool,
    pub subtests: Vec<SubtestComparison>,
    /// Geometric mean of the scored subtests' `rel`, per run.
    pub rel: Vec<f64>,
    pub best: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverallComparison {
    pub rel: Vec<f64>,
    /// Run indices, best first.
    pub ranking: Vec<usize>,
    pub summary: String,
}

/// How much of the available measurement the verdict actually rests on.
///
/// A comparison that quietly drops twenty-eight of thirty subtests and then
/// announces a winner is worse than no comparison: the number looks like the
/// others and means far less. The counts travel with the result so the reader
/// can weigh it, and the summary says so in words when most of it is missing.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Coverage {
    pub compared: usize,
    pub left_out: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub schema: &'static str,
    pub runs: Vec<RunRef>,
    /// Everything that was dropped, and why. Never empty for a reason the
    /// reader would want to know about.
    pub warnings: Vec<String>,
    pub coverage: Coverage,
    pub components: Vec<ComponentComparison>,
    pub overall: OverallComparison,
}

/// One measurement as the index holds it.
#[derive(Debug, Clone)]
struct Measurement {
    component: String,
    component_label: Option<String>,
    component_graded: bool,
    id: String,
    label: Option<String>,
    unit: String,
    direction: Option<String>,
    representative: Option<String>,
    value: f64,
    scored: bool,
}

/// Build a comparison of the given runs, in the order given.
///
/// The first run is the reference every ratio is against, which is why order
/// is the caller's to choose rather than something this sorts.
pub fn compare(conn: &Connection, run_ids: &[i64]) -> Result<Comparison> {
    check_request(run_ids)?;

    let runs: Vec<RunRef> = run_ids
        .iter()
        .map(|id| load_run(conn, *id))
        .collect::<Result<_>>()?;
    let metrics: Vec<Vec<Measurement>> = run_ids
        .iter()
        .map(|id| load_measurements(conn, *id))
        .collect::<Result<_>>()?;

    let mut warnings = configuration_warnings(&runs);
    let mut left_out = 0usize;
    let components = build_components(&metrics, &mut warnings, &mut left_out);
    let coverage = Coverage {
        compared: components.iter().map(|c| c.subtests.len()).sum(),
        left_out,
    };

    if components.is_empty() {
        bail!(
            "these runs share no comparable measurements. {}",
            if warnings.is_empty() {
                "They have no subtests in common.".to_string()
            } else {
                warnings.join(" ")
            }
        );
    }

    let n = runs.len();
    // Graded components only. The rest are shown with their own figures and
    // kept out of the verdict, exactly as they are kept out of a grade.
    let graded: Vec<&ComponentComparison> = components.iter().filter(|c| c.graded).collect();
    let counted: Vec<&ComponentComparison> = if graded.is_empty() {
        // Nothing gradeable in common — better to rank on what there is than
        // to refuse, as long as the caller can see which components those are.
        warnings.push(
            "No graded component is common to these runs, so the overall figure is built from \
             the ungraded ones — network and GPU depend on the host and its drivers as much as \
             on the machine."
                .to_string(),
        );
        components.iter().collect()
    } else {
        graded
    };
    let overall_rel: Vec<f64> = (0..n)
        .map(|i| geomean(&counted.iter().map(|c| c.rel[i]).collect::<Vec<_>>()))
        .collect();
    let mut ranking: Vec<usize> = (0..n).collect();
    ranking.sort_by(|&a, &b| overall_rel[b].total_cmp(&overall_rel[a]));
    let summary = summarize(&runs, &components, &overall_rel, &ranking, coverage);

    Ok(Comparison {
        schema: COMPARE_SCHEMA,
        runs,
        warnings,
        coverage,
        components,
        overall: OverallComparison {
            rel: overall_rel,
            ranking,
            summary,
        },
    })
}

/// Turn what somebody typed into run ids.
///
/// A bare number is a run id; anything else is a machine, meaning its latest
/// run. The ambiguity is real but narrow — a hostname made only of digits —
/// and worth it, because the useful thing to type is a machine name and the
/// useful thing to *script* is an id.
pub fn resolve(conn: &Connection, names: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for name in names {
        if let Ok(id) = name.parse::<i64>() {
            ids.push(id);
            continue;
        }
        let found: Option<i64> = conn
            .query_row(
                "SELECT id FROM run
                   WHERE machine_key = ?1 COLLATE NOCASE OR hostname = ?1 COLLATE NOCASE
                   ORDER BY taken_at DESC, id DESC LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .ok();
        match found {
            Some(id) => ids.push(id),
            None => bail!(
                "no machine in the index matches {name:?}. Try the hostname, the machine key the \
                 dashboard shows on the drilldown, or a run id."
            ),
        }
    }
    Ok(ids)
}

/// Everything wrong with a request that can be judged without looking at the
/// index at all.
///
/// Separate so the HTTP layer can run it *before* it checks who may see what.
/// These answers depend only on the request, so giving them first tells the
/// caller nothing about the estate — whereas resolving ids first would turn
/// "that is too many runs" into "that run does not exist", which is both
/// unhelpful and a worse thing to say.
pub fn check_request(run_ids: &[i64]) -> Result<()> {
    if run_ids.len() < 2 {
        bail!("a comparison needs at least two runs");
    }
    if run_ids.len() > MAX_RUNS {
        bail!(
            "a comparison holds at most {MAX_RUNS} runs; {} were asked for. More than that \
             cannot be read side by side.",
            run_ids.len()
        );
    }
    if let Some(dup) = first_duplicate(run_ids) {
        bail!("run {dup} appears twice — a run is not worth comparing with itself");
    }
    Ok(())
}

fn first_duplicate(ids: &[i64]) -> Option<i64> {
    let mut seen = Vec::new();
    for id in ids {
        if seen.contains(id) {
            return Some(*id);
        }
        seen.push(*id);
    }
    None
}

fn load_run(conn: &Connection, run_id: i64) -> Result<RunRef> {
    let row = conn.query_row(
        "SELECT machine_key, hostname, taken_at, cpu_model, preset, profile, baseline,
                build_isa, overall_score, overall_grade
           FROM run WHERE id = ?1",
        [run_id],
        |r| {
            let machine_key: String = r.get(0)?;
            let hostname: Option<String> = r.get(1)?;
            Ok(RunRef {
                run_id,
                label: hostname
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| machine_key.clone()),
                machine_key,
                taken_at: r.get(2)?,
                cpu_model: r.get(3)?,
                comparability: Comparability {
                    preset: r.get(4)?,
                    profile: r.get(5)?,
                    baseline: r.get(6)?,
                    build_isa: r.get(7)?,
                },
                overall_score: r.get(8)?,
                overall_grade: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            })
        },
    );
    match row {
        Ok(run) => Ok(run),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            bail!("there is no run {run_id} in the index")
        }
        Err(e) => Err(e.into()),
    }
}

fn load_measurements(conn: &Connection, run_id: i64) -> Result<Vec<Measurement>> {
    let mut stmt = conn.prepare(
        "SELECT s.component, c.label, s.id, s.label, s.unit, s.direction, s.representative,
                s.value, s.scored, COALESCE(c.graded, 0)
           FROM subtest s
           LEFT JOIN component c ON c.run_id = s.run_id AND c.id = s.component
          WHERE s.run_id = ?1
          ORDER BY s.component, s.id",
    )?;
    let rows = stmt.query_map([run_id], |r| {
        Ok(Measurement {
            component: r.get(0)?,
            component_label: r.get(1)?,
            id: r.get(2)?,
            label: r.get(3)?,
            unit: r.get(4)?,
            direction: r.get(5)?,
            representative: r.get(6)?,
            value: r.get(7)?,
            scored: r.get(8)?,
            component_graded: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Components in the order the first run has them, keeping only what every run
/// can answer for.
fn build_components(
    metrics: &[Vec<Measurement>],
    warnings: &mut Vec<String>,
    left_out: &mut usize,
) -> Vec<ComponentComparison> {
    let n = metrics.len();
    let mut components = Vec::new();

    let mut seen_components: Vec<String> = Vec::new();
    for m in &metrics[0] {
        if !seen_components.contains(&m.component) {
            seen_components.push(m.component.clone());
        }
    }

    for component in seen_components {
        let mut subtests = Vec::new();
        for first in metrics[0].iter().filter(|m| m.component == component) {
            let per_run: Option<Vec<&Measurement>> = metrics
                .iter()
                .map(|run| {
                    run.iter()
                        .find(|m| m.component == component && m.id == first.id)
                })
                .collect();
            let Some(per_run) = per_run else {
                warnings.push(format!(
                    "{}/{} is not in every run — left out.",
                    component, first.id
                ));
                *left_out += 1;
                continue;
            };

            let Some(direction) = per_run[0].direction.clone() else {
                warnings.push(format!(
                    "{}/{} has no direction recorded, so a ratio would have no sign — left out. \
                     Runs indexed before 0.5.3 have none; `scan --reindex` fills them in from \
                     the documents.",
                    component, first.id
                ));
                *left_out += 1;
                continue;
            };
            if per_run
                .iter()
                .any(|m| m.direction.as_ref() != Some(&direction))
            {
                warnings.push(format!(
                    "{}/{} is measured in different directions across these runs — left out.",
                    component, first.id
                ));
                *left_out += 1;
                continue;
            }

            // A peak against a median measures the statistic, not the machine.
            //
            // **Absent means median.** `representative` arrived in loadbearer
            // 1.5.0 and everything written before it was a median, so a 1.2.4
            // run compares perfectly well with a modern one. Reading the
            // missing field as "some other statistic" refuses every comparison
            // that spans that release — which is what it did until a real pair
            // of documents went through it.
            fn statistic(m: &Measurement) -> &str {
                m.representative.as_deref().unwrap_or("median")
            }
            let representative = per_run[0].representative.clone();
            if per_run
                .iter()
                .any(|m| statistic(m) != statistic(per_run[0]))
            {
                warnings.push(format!(
                    "{}/{} is a peak in one run and a median in another — left out, because the \
                     difference would be the statistic rather than the machine.",
                    component, first.id
                ));
                *left_out += 1;
                continue;
            }

            let values: Vec<f64> = per_run.iter().map(|m| m.value).collect();
            let reference = values[0];
            let rel: Vec<f64> = values
                .iter()
                .map(|v| goodness(*v, reference, &direction))
                .collect();
            let best = argmax(&rel);
            subtests.push(SubtestComparison {
                id: first.id.clone(),
                label: first.label.clone().unwrap_or_else(|| first.id.clone()),
                unit: first.unit.clone(),
                direction,
                representative,
                values,
                rel,
                best,
                // Scored only where *every* run scored it: a subtest that
                // counted for one machine and not another would weight the
                // rollup differently per machine.
                scored: per_run.iter().all(|m| m.scored),
            });
        }

        let scored: Vec<&SubtestComparison> = subtests.iter().filter(|s| s.scored).collect();
        if scored.is_empty() {
            if !subtests.is_empty() {
                warnings.push(format!(
                    "{component} has no scored subtest in common, so it has no rollup — its \
                     measurements are shown on their own."
                ));
            }
            continue;
        }

        let rel: Vec<f64> = (0..n)
            .map(|i| geomean(&scored.iter().map(|s| s.rel[i]).collect::<Vec<_>>()))
            .collect();
        let best = argmax(&rel);
        let label = metrics[0]
            .iter()
            .find(|m| m.component == component)
            .and_then(|m| m.component_label.clone())
            .unwrap_or_else(|| component.clone());
        let graded = metrics[0]
            .iter()
            .find(|m| m.component == component)
            .map(|m| m.component_graded)
            .unwrap_or(false);
        components.push(ComponentComparison {
            id: component,
            label,
            graded,
            subtests,
            rel,
            best,
        });
    }
    components
}

/// Differences that change what was measured rather than how well.
fn configuration_warnings(runs: &[RunRef]) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, values: Vec<&str>| {
        let mut distinct: Vec<&str> = Vec::new();
        for v in values {
            if !distinct.contains(&v) {
                distinct.push(v);
            }
        }
        if distinct.len() > 1 {
            out.push(format!(
                "These runs used different {what}s ({}). Results are only strictly comparable \
                 within one.",
                distinct.join(", ")
            ));
        }
    };
    check(
        "duration preset",
        runs.iter()
            .map(|r| r.comparability.preset.as_str())
            .collect(),
    );
    check(
        "profile",
        runs.iter()
            .map(|r| r.comparability.profile.as_str())
            .collect(),
    );
    check(
        "instruction set",
        runs.iter()
            .map(|r| r.comparability.build_isa.as_str())
            .collect(),
    );
    out
}

/// How much better `value` is than `reference`, given which way is better.
fn goodness(value: f64, reference: f64, direction: &str) -> f64 {
    if value <= 0.0 || reference <= 0.0 {
        return 1.0;
    }
    let ratio = value / reference;
    if direction == "lower_is_better" {
        1.0 / ratio
    } else {
        ratio
    }
}

fn geomean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let sum_ln: f64 = values.iter().map(|v| v.max(1e-9).ln()).sum();
    (sum_ln / values.len() as f64).exp()
}

fn argmax(xs: &[f64]) -> usize {
    xs.iter()
        .enumerate()
        .fold(0, |best, (i, v)| if *v > xs[best] { i } else { best })
}

/// One sentence a reader can act on, rather than a table they have to add up.
fn summarize(
    runs: &[RunRef],
    components: &[ComponentComparison],
    overall_rel: &[f64],
    ranking: &[usize],
    coverage: Coverage,
) -> String {
    let lead = ranking[0];
    // Said first, because it changes how everything after it should be read.
    //
    // Two ways a verdict can be thin, and only one of them is about what was
    // dropped: an old run may simply *have* few subtests, so a comparison can
    // rest on one measurement with nothing left out at all. A real pair of
    // fixtures did exactly that and announced a 29% lead from a single number.
    let total = coverage.compared + coverage.left_out;
    let caveat = if coverage.left_out > coverage.compared {
        format!(
            "Based on {} of {total} measurements — most were not comparable, see the warnings. ",
            coverage.compared,
        )
    } else if coverage.compared < THIN_COMPARISON {
        format!(
            "Based on {} measurement{} — too few to read as a general verdict. ",
            coverage.compared,
            if coverage.compared == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };

    if runs.len() == 2 {
        let other = ranking[1];
        let advantage = overall_rel[lead] / overall_rel[other].max(1e-9) - 1.0;
        // Below this the difference is run-to-run noise on most hardware, and
        // calling it a win would be reading spread as signal.
        if advantage < 0.03 {
            return format!(
                "{caveat}{} and {} are within 3% overall — effectively equal.",
                runs[0].label, runs[1].label
            );
        }
        let mut lead_wins = Vec::new();
        let mut other_wins = Vec::new();
        for c in components.iter().filter(|c| c.graded) {
            let adv = c.rel[lead] / c.rel[other].max(1e-9) - 1.0;
            if adv > 0.02 {
                lead_wins.push(format!("{} +{:.0}%", c.label.to_lowercase(), adv * 100.0));
            } else if adv < -0.02 {
                other_wins.push(format!("{} +{:.0}%", c.label.to_lowercase(), -adv * 100.0));
            }
        }
        let mut s = format!(
            "{caveat}{} leads by {:.0}% overall",
            runs[lead].label,
            advantage * 100.0
        );
        if !lead_wins.is_empty() {
            s.push_str(&format!(" (ahead on {})", lead_wins.join(", ")));
        }
        if !other_wins.is_empty() {
            s.push_str(&format!(
                "; {} wins {}",
                runs[other].label,
                other_wins.join(", ")
            ));
        }
        s.push('.');
        return s;
    }

    let parts: Vec<String> = ranking
        .iter()
        .enumerate()
        .map(|(rank, &i)| {
            if rank == 0 {
                format!("1. {}", runs[i].label)
            } else {
                let behind = overall_rel[i] / overall_rel[lead].max(1e-9) - 1.0;
                format!("{}. {} ({:.0}%)", rank + 1, runs[i].label, behind * 100.0)
            }
        })
        .collect();
    format!("{caveat}Ranking (overall vs leader): {}", parts.join("   "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn indexed(files: &[&str]) -> Index {
        let mut idx = Index::open_in_memory().expect("index");
        for f in files {
            idx.ingest_file(&fixture(f)).expect("fixture");
        }
        idx
    }

    fn run_ids(idx: &Index) -> Vec<i64> {
        let mut stmt = idx
            .conn()
            .prepare("SELECT id FROM run ORDER BY id")
            .unwrap();
        let ids = stmt.query_map([], |r| r.get(0)).unwrap();
        ids.collect::<rusqlite::Result<Vec<i64>>>().unwrap()
    }

    /// The whole point of storing `direction`: a lower-is-better metric that
    /// gets *smaller* has improved, and a ratio that ignores that says the
    /// opposite.
    #[test]
    fn direction_decides_which_way_is_better() {
        assert!((goodness(200.0, 100.0, "higher_is_better") - 2.0).abs() < 1e-9);
        assert!((goodness(50.0, 100.0, "lower_is_better") - 2.0).abs() < 1e-9);
        // An unknown direction is treated as higher-is-better rather than
        // refused here — `build_components` never passes one through, because
        // it declines the subtest first.
        assert!((goodness(200.0, 100.0, "something-else") - 2.0).abs() < 1e-9);
        // Nothing is knowable from a zero or a negative.
        assert_eq!(goodness(0.0, 100.0, "higher_is_better"), 1.0);
    }

    #[test]
    fn two_real_runs_compare_and_rank() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);
        let c = compare(idx.conn(), &ids).expect("comparison");

        assert_eq!(c.schema, COMPARE_SCHEMA);
        assert_eq!(c.runs.len(), 2);
        assert!(!c.components.is_empty(), "no components in common");
        // Every ratio is against run 0, so run 0 is 1.0 by construction.
        for component in &c.components {
            assert!((component.rel[0] - 1.0).abs() < 1e-9);
            for s in &component.subtests {
                assert!((s.rel[0] - 1.0).abs() < 1e-9);
            }
        }
        assert!((c.overall.rel[0] - 1.0).abs() < 1e-9);
        assert_eq!(c.overall.ranking.len(), 2);
        // The modern laptop is the faster of the two, and the summary says so
        // in words rather than leaving it to be read off a table.
        assert_eq!(c.overall.ranking[0], 0, "{}", c.overall.summary);
        assert!(
            c.overall.summary.contains("FLEET-WIN-01"),
            "{}",
            c.overall.summary
        );
    }

    /// A comparison needs at least two runs, at most four, and never the same
    /// run twice — each of which is a request that cannot mean anything.
    #[test]
    fn impossible_requests_are_refused_with_the_reason() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);

        let one = compare(idx.conn(), &ids[..1]).unwrap_err().to_string();
        assert!(one.contains("at least two"), "{one}");

        let same = compare(idx.conn(), &[ids[0], ids[0]])
            .unwrap_err()
            .to_string();
        assert!(same.contains("twice"), "{same}");

        let many: Vec<i64> = std::iter::repeat_n(ids[0], MAX_RUNS + 1)
            .enumerate()
            .map(|(i, id)| id + i as i64)
            .collect();
        let too_many = compare(idx.conn(), &many).unwrap_err().to_string();
        assert!(too_many.contains("at most"), "{too_many}");

        let missing = compare(idx.conn(), &[ids[0], 9999])
            .unwrap_err()
            .to_string();
        assert!(missing.contains("no run 9999"), "{missing}");
    }

    /// A run indexed before `direction` was recorded cannot be compared, and
    /// saying so is the whole difference between this and a wrong answer.
    #[test]
    fn a_run_without_direction_is_declined_rather_than_guessed() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);
        // Exactly what an index written before 0.5.3 holds.
        idx.conn()
            .execute("UPDATE subtest SET direction = NULL", [])
            .unwrap();

        let err = compare(idx.conn(), &ids).unwrap_err().to_string();
        assert!(
            err.contains("no comparable measurements"),
            "it should refuse outright: {err}"
        );
        assert!(err.contains("--reindex"), "and name the fix: {err}");
    }

    /// A run from before `representative` existed is still comparable: the
    /// field arrived in loadbearer 1.5.0 and everything older was a median.
    ///
    /// This is the case that a unit test built from two modern fixtures could
    /// not see. Reading the absent field as "some other statistic" refused
    /// every subtest of every comparison spanning that release — the whole
    /// comparison, not a corner of it — and it took putting real documents
    /// through the running server to notice.
    #[test]
    fn a_run_from_before_peak_reporting_still_compares() {
        let idx = indexed(&["legacy-1.2.4.json", "low-end.json"]);
        let ids = run_ids(&idx);

        let missing: i64 = idx
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE run_id = ?1 AND representative IS NULL",
                [ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(missing > 0, "the legacy fixture should have no statistic");

        let c = compare(idx.conn(), &ids).expect("a legacy run is still comparable");
        assert!(c.coverage.compared > 0, "{:?}", c.warnings);
        assert!(
            !c.warnings.iter().any(|w| w.contains("peak in one run")),
            "absent is median, not a mismatch: {:?}",
            c.warnings
        );
    }

    /// 1.5.0 added peak reporting, so an estate can hold both statistics. A
    /// peak against a median measures the statistic, not the machine —
    /// loadbearer's own `compare` does not guard this, and it should.
    #[test]
    fn a_peak_is_not_compared_against_a_median() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);
        idx.conn()
            .execute(
                "UPDATE subtest SET representative = 'peak' WHERE run_id = ?1",
                [ids[1]],
            )
            .unwrap();

        let c = compare(idx.conn(), &ids).expect("what survives is still comparable");
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("peak in one run and a median in another")),
            "{:?}",
            c.warnings
        );
        // The two multi-core subtests are peak in *both* runs, so they survive
        // — and the verdict then rests on two measurements out of thirty. That
        // is exactly the case `coverage` exists to make visible, rather than
        // presenting the same confident sentence as a full comparison.
        assert!(
            c.coverage.left_out > c.coverage.compared,
            "{:?}",
            c.coverage
        );
        assert!(
            c.overall.summary.starts_with("Based on"),
            "the summary must lead with how little it rests on: {}",
            c.overall.summary
        );
    }

    /// Different presets measure different things. Worth comparing sometimes,
    /// never worth comparing silently.
    #[test]
    fn a_different_preset_is_warned_about_and_still_compared() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);
        // Both fixtures are `thorough`, so this has to be a preset neither of
        // them already uses or the test proves nothing.
        idx.conn()
            .execute("UPDATE run SET preset = 'short' WHERE id = ?1", [ids[1]])
            .unwrap();

        let c = compare(idx.conn(), &ids).expect("still comparable");
        assert!(
            c.warnings.iter().any(|w| w.contains("duration preset")),
            "{:?}",
            c.warnings
        );
        assert!(!c.components.is_empty(), "and it should still produce one");
    }

    /// Network and GPU are measured, shown, and kept out of the verdict — the
    /// same rule the grade follows.
    ///
    /// Found by putting three real machines through the running server: the
    /// ranking disagreed with the scores on the dashboard, because one laptop's
    /// eightfold-worse network dragged its geometric mean below a machine it
    /// beats on every piece of silicon.
    #[test]
    fn an_ungraded_component_is_shown_but_does_not_decide_the_verdict() {
        let idx = indexed(&["win-modern.json", "linux-throttled.json"]);
        let ids = run_ids(&idx);
        let c = compare(idx.conn(), &ids).expect("comparison");

        let network = c
            .components
            .iter()
            .find(|c| c.id == "network")
            .expect("the fixtures measure a network");
        assert!(!network.graded, "network must not be graded");
        assert!(
            !network.rel.iter().all(|r| (*r - 1.0).abs() < 1e-9),
            "and it should still carry its own figures"
        );

        // The overall figure is the graded components only.
        let expected: Vec<f64> = (0..2)
            .map(|i| {
                geomean(
                    &c.components
                        .iter()
                        .filter(|c| c.graded)
                        .map(|c| c.rel[i])
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        for (i, want) in expected.iter().enumerate() {
            assert!(
                (c.overall.rel[i] - want).abs() < 1e-9,
                "overall {i} is {} but the graded components give {want}",
                c.overall.rel[i]
            );
        }
    }

    /// A verdict resting on one measurement has to say so, even when nothing
    /// was dropped to get there.
    ///
    /// The 1.2.4-era fixture simply *has* few subtests, so comparing it with a
    /// modern run produced "leads by 29% overall" from a single number, with an
    /// empty warnings list and a coverage caveat that never fired because it
    /// only looked at what had been left out.
    #[test]
    fn a_thin_comparison_says_so_before_it_says_anything_else() {
        let idx = indexed(&["legacy-1.2.4.json", "low-end.json"]);
        let ids = run_ids(&idx);
        let c = compare(idx.conn(), &ids).expect("comparison");

        assert!(c.coverage.compared < THIN_COMPARISON, "{:?}", c.coverage);
        assert!(
            c.overall.summary.starts_with("Based on"),
            "a one-measurement verdict must lead with that: {}",
            c.overall.summary
        );
    }

    /// An informational subtest is measured and shown but has no baseline, so
    /// folding it into the rollup would weight a number that means something
    /// different.
    #[test]
    fn informational_subtests_stay_out_of_the_rollup() {
        let idx = indexed(&["win-modern.json", "low-end.json"]);
        let ids = run_ids(&idx);
        let c = compare(idx.conn(), &ids).expect("comparison");

        for component in &c.components {
            let scored: Vec<f64> = component
                .subtests
                .iter()
                .filter(|s| s.scored)
                .map(|s| s.rel[1])
                .collect();
            if scored.is_empty() {
                continue;
            }
            let expected = geomean(&scored);
            assert!(
                (component.rel[1] - expected).abs() < 1e-9,
                "{} rolled up {} but its scored subtests give {expected}",
                component.id,
                component.rel[1]
            );
        }
    }
}
