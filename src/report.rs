//! A preflight report: what was proved, what was not, and what cannot be.
//!
//! Shared by `check-auth` and `service preflight`, because both answer the
//! same shape of question — a list of things an operator would otherwise
//! have to work out by hand, each with the fix attached rather than left as
//! an exercise. Both exit non-zero when anything failed, so either can gate
//! a deploy rather than needing careful reading.

/// One `check-auth` run: what was proved, what was not, and what cannot be.
#[derive(Default)]
pub struct Report {
    checks: Vec<(Outcome, &'static str, String, Option<String>)>,
    pub(crate) unknown: Vec<String>,
}

#[derive(PartialEq)]
enum Outcome {
    Pass,
    Fail,
    Note,
}

impl Report {
    pub(crate) fn pass(&mut self, what: &'static str, detail: String) {
        self.checks.push((Outcome::Pass, what, detail, None));
    }
    pub(crate) fn note(&mut self, what: &'static str, detail: String) {
        self.checks.push((Outcome::Note, what, detail, None));
    }
    /// `advice` takes an owned string so it can name the actual account and
    /// the actual directory. Advice with a `<placeholder>` in it is advice
    /// somebody has to translate before they can use it.
    pub(crate) fn fail(&mut self, what: &'static str, detail: String, advice: impl Into<String>) {
        self.checks
            .push((Outcome::Fail, what, detail, Some(advice.into())));
    }

    /// Whether anything failed, so the command can exit non-zero and a deploy
    /// script can stop rather than press on to a sign-in that will not work.
    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|(o, ..)| *o == Outcome::Fail)
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (outcome, what, detail, advice) in &self.checks {
            let mark = match outcome {
                Outcome::Pass => "ok  ",
                Outcome::Fail => "FAIL",
                Outcome::Note => "note",
            };
            writeln!(f, "  {mark}  {what:<18} {detail}")?;
            if let Some(advice) = advice {
                for line in wrap(advice, 68) {
                    writeln!(f, "                           {line}")?;
                }
            }
        }
        if !self.unknown.is_empty() {
            // Not "without a real sign-in": this type serves the service
            // preflight too, where the leftovers are about shares and accounts.
            writeln!(f, "\nNot knowable from here:")?;
            for item in &self.unknown {
                let mut lines = wrap(item, 72).into_iter();
                if let Some(first) = lines.next() {
                    writeln!(f, "  - {first}")?;
                }
                for line in lines {
                    writeln!(f, "    {line}")?;
                }
            }
        }
        Ok(())
    }
}

/// Greedy wrap. The advice is prose and a terminal is not always wide.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}
