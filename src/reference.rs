//! The command-line and configuration reference, generated from the
//! definitions rather than written alongside them.
//!
//! A hand-maintained reference table is a second copy of something that
//! already exists, with nothing checking the two agree — so it goes stale one
//! flag at a time and is worse than no table at all, because a reader trusts
//! it. Here the command tables come from the `clap` definition and the
//! configuration section embeds `Config::starter()` verbatim, so neither can
//! drift from what the binary actually accepts.
//!
//! `loadbearer-fleet reference` prints it. The wiki page is that output; the
//! tests below enforce what a generator can't — that nothing in either surface
//! is undocumented in the first place.

use std::fmt::Write as _;

use clap::{Arg, Command};

use crate::config::Config;

/// The whole reference, as GitHub-flavoured Markdown.
pub fn markdown(mut root: Command) -> String {
    // Globals are attached to the root and propagated to subcommands during
    // build, so build first and then list them once rather than in every
    // command's table.
    root.build();

    let mut s = String::with_capacity(16 * 1024);
    let name = root.get_name().to_string();

    let _ = writeln!(s, "# Command line and configuration");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Generated from the definitions in the source by `{name} reference`, so it \
         cannot drift from what the binary accepts. If this page and `--help` ever \
         disagree, `--help` is right and this page is stale — please open an issue."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "{PRECEDENCE}");

    // Commands index.
    let _ = writeln!(s, "## Commands");
    let _ = writeln!(s);
    let _ = writeln!(s, "| Command | What it does |");
    let _ = writeln!(s, "| --- | --- |");
    for sub in visible(&root) {
        let _ = writeln!(
            s,
            "| [`{}`](#{}) | {} |",
            sub.get_name(),
            anchor(&format!("{} {}", name, sub.get_name())),
            cell(sub.get_about().map(|a| a.to_string()).unwrap_or_default())
        );
    }
    let _ = writeln!(s);

    // Global options, once.
    let globals: Vec<&Arg> = root
        .get_arguments()
        .filter(|a| a.is_global_set() && a.get_id() != "help" && a.get_id() != "version")
        .collect();
    if !globals.is_empty() {
        let _ = writeln!(s, "## Global options");
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "Accepted by every command, and shown once here rather than repeated below."
        );
        let _ = writeln!(s);
        options_table(&mut s, &globals);
        let _ = writeln!(s);
    }

    // Depth 1 so a top-level command is an `##`, level with "Commands" above
    // rather than reading as a subsection of it.
    for sub in visible(&root) {
        command_section(&mut s, &name, sub, 1);
    }

    let _ = writeln!(s, "{CONFIG_INTRO}");
    let _ = writeln!(s, "```toml");
    let _ = write!(s, "{}", Config::starter());
    let _ = writeln!(s, "```");
    let _ = writeln!(s);
    let _ = writeln!(s, "{CONFIG_NOTES}");

    s
}

/// One command, and its own subcommands one level deeper.
fn command_section(s: &mut String, path: &str, cmd: &Command, depth: usize) {
    let full = format!("{path} {}", cmd.get_name());
    let hashes = "#".repeat(depth + 1);
    let _ = writeln!(s, "{hashes} `{full}`");
    let _ = writeln!(s);

    // The long about carries the reasoning; the short one is the table cell.
    let about = cmd
        .get_long_about()
        .or_else(|| cmd.get_about())
        .map(|a| a.to_string())
        .unwrap_or_default();
    if !about.is_empty() {
        // Same missing full stop as in a table cell, for the same reason —
        // clap strips it from the short form.
        let mut about = about.trim().to_string();
        if ends_a_sentence(&about) {
            about.push('.');
        }
        let _ = writeln!(s, "{about}");
        let _ = writeln!(s);
    }

    let positionals: Vec<&Arg> = cmd.get_arguments().filter(|a| a.is_positional()).collect();
    if !positionals.is_empty() {
        let _ = writeln!(s, "| Argument | Required | What it is |");
        let _ = writeln!(s, "| --- | --- | --- |");
        for a in &positionals {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} |",
                value_name(a),
                if a.is_required_set() { "yes" } else { "no" },
                help_of(a)
            );
        }
        let _ = writeln!(s);
    }

    let opts: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| {
            !a.is_positional()
                && !a.is_global_set()
                && a.get_id() != "help"
                && a.get_id() != "version"
        })
        .collect();
    if !opts.is_empty() {
        options_table(s, &opts);
        let _ = writeln!(s);
    }

    for nested in visible(cmd) {
        command_section(s, &full, nested, depth + 1);
    }
}

fn options_table(s: &mut String, args: &[&Arg]) {
    let _ = writeln!(s, "| Option | What it does |");
    let _ = writeln!(s, "| --- | --- |");
    for a in args {
        let mut flag = String::new();
        if let Some(short) = a.get_short() {
            let _ = write!(flag, "-{short}, ");
        }
        if let Some(long) = a.get_long() {
            let _ = write!(flag, "--{long}");
        }
        if a.get_action().takes_values() {
            let _ = write!(flag, " <{}>", value_name(a));
        }
        let mut text = help_of(a);
        // Only for options that take a value. A boolean flag reports a default
        // of `false`, which is both obvious and misleading — it reads as a
        // setting you could pass `true` to.
        if a.get_action().takes_values() {
            let defaults: Vec<String> = a
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().to_string())
                .collect();
            if !defaults.is_empty() {
                let _ = write!(text, " Defaults to `{}`.", defaults.join(" "));
            }
        }
        let _ = writeln!(s, "| `{}` | {} |", flag.trim(), text);
    }
}

/// Subcommands worth documenting: `help` is clap's own and says nothing about
/// this tool.
fn visible(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help" && !c.is_hide_set())
}

fn value_name(a: &Arg) -> String {
    a.get_value_names()
        .and_then(|n| n.first())
        .map(|n| n.to_string())
        .unwrap_or_else(|| a.get_id().to_string().to_uppercase())
}

fn help_of(a: &Arg) -> String {
    cell(
        a.get_long_help()
            .or_else(|| a.get_help())
            .map(|h| h.to_string())
            .unwrap_or_default(),
    )
}

/// A table cell can hold neither a newline nor an unescaped pipe. Doc comments
/// wrap, so both happen.
///
/// The full stop is put back because clap strips it from the short help, which
/// reads fine in a terminal column and badly in prose — especially where
/// something else is appended after it.
fn cell(text: String) -> String {
    let mut flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if ends_a_sentence(&flat) {
        flat.push('.');
    }
    flat.replace('|', r"\|")
}

/// Whether a full stop is missing. A closing backtick or bracket counts: clap
/// strips only the final `.`, so `` ...as `serve --allow-remote`. `` arrives
/// here ending in a backtick and is just as much a finished sentence.
fn ends_a_sentence(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '`' || c == ')')
}

/// GitHub's heading slug for a `` `name` `` heading: backticks and other
/// punctuation dropped, spaces to hyphens.
fn anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .replace(' ', "-")
}

const PRECEDENCE: &str = "\
## What wins

Settings come from three places, and the nearer one wins: **the command line**,
then **the configuration file**, then the built-in default. Logging has a fourth
in front of the others — `RUST_LOG` beats `--log-level` beats `[log] level` —
because the environment is what someone reaches for while debugging a service
they cannot easily reconfigure.

Two things about paths in the configuration file, both of which exist because a
service does not start where you do — Windows starts one in `System32` and
systemd in `/`:

- **Relative paths resolve beside the configuration file**, not against the
  working directory, so the answer is the same however the process was started.
- **`service install` refuses a relative `--config`**, for the same reason.
";

const CONFIG_INTRO: &str = "\
## The configuration file

Everything above can also be set on the command line; the file exists because a
service has no command line anyone reads, and because the sign-in settings are
too many to be flags.

`loadbearer-fleet init-config` writes the file below. It is the reference as
well as the starting point — this section embeds its output verbatim, so the
comments here are the ones the binary itself emits.
";

const CONFIG_NOTES: &str = "\
### Notes on the file

- **A placeholder is refused, not ignored.** Every value that must be filled in
  carries `PUT-`, and loading a file that still contains one fails naming the
  field. The path settings are checked before the `auth.mode = \"none\"`
  shortcut, because sign-in is the last thing anyone configures and an
  unedited `log.file` would otherwise create a directory with that name and log
  into it contentedly.
- **`auth.mode = \"none\"` makes everyone who can reach the port an
  administrator.** That is why a non-loopback bind is refused without sign-in
  and an https `public_url`, and why the dashboard header says \"Local access\"
  rather than implying somebody signed in.
- **`[log] file` is required to run as a service** and `service install`
  refuses without it: a service has no console, so with no log file there is no
  way at all to find out why it did not start. The log rotates daily, with the
  date appended to the name.
- **`client_secret` is normally empty.** The intended shape is a public client
  with PKCE, which needs no secret — and a secret in a configuration file on a
  management server is a secret in a backup.
- **Unknown keys are rejected.** A typo in a key name fails at load rather than
  being silently ignored, which is the failure mode where a setting you think
  you configured was never read.
";

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn root() -> Command {
        crate::Cli::command()
    }

    /// Walk every command, including nested ones.
    fn walk(cmd: &Command, path: String, out: &mut Vec<(String, Command)>) {
        for sub in visible(cmd) {
            let full = format!("{path} {}", sub.get_name());
            out.push((full.clone(), sub.clone()));
            walk(sub, full, out);
        }
    }

    fn all_commands() -> Vec<(String, Command)> {
        let mut out = Vec::new();
        let mut r = root();
        r.build();
        walk(&r, "loadbearer-fleet".to_string(), &mut out);
        out
    }

    /// The generator can only render help that exists. This is the drift that
    /// actually happens: a flag added in a hurry with no doc comment, which
    /// then shows up as a blank cell in the reference and a bare `--flag` in
    /// `--help`.
    #[test]
    fn every_command_and_argument_carries_help() {
        let mut missing = Vec::new();
        for (path, cmd) in all_commands() {
            if cmd.get_about().is_none() {
                missing.push(format!("{path} — the command itself"));
            }
            for a in cmd.get_arguments() {
                if a.get_id() == "help" || a.get_id() == "version" || a.is_global_set() {
                    continue;
                }
                if a.get_help().is_none() && a.get_long_help().is_none() {
                    missing.push(format!("{path} — {}", a.get_id()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "undocumented, so they would render as empty cells:\n  {}",
            missing.join("\n  ")
        );
    }

    /// Every command and long option reaches the page. Catches a filter here
    /// quietly dropping something the CLI does have.
    #[test]
    fn the_reference_names_every_command_and_option() {
        let page = markdown(root());
        for (path, cmd) in all_commands() {
            let name = path.rsplit(' ').next().unwrap().to_string();
            assert!(
                page.contains(&format!("`{path}`")),
                "{path} is missing from the reference"
            );
            for a in cmd.get_arguments() {
                if a.is_global_set() || a.get_id() == "help" || a.get_id() == "version" {
                    continue;
                }
                if let Some(long) = a.get_long() {
                    assert!(
                        page.contains(&format!("--{long}")),
                        "--{long} (of {name}) is missing from the reference"
                    );
                }
            }
        }
    }

    /// The configuration half is only drift-proof because it embeds the starter
    /// file. That holds only while the starter file itself covers every key.
    ///
    /// The comparison is against a Config with **every field written out
    /// explicitly** rather than `Config::default()`, and that is the whole
    /// trick. TOML has no null, so the serializer omits a `None`, which meant
    /// an `Option` field was invisible here — the guard silently covered only
    /// the fields that happened to have a value, and `collection_dir`,
    /// `archive_dir`, `log.file` and `ca_bundle` were all of that shape.
    /// Listing the fields with no `..Default::default()` makes the *compiler*
    /// enforce completeness: a new setting anywhere in the config will not
    /// build until it is named here, and then this test asks whether
    /// `init-config` mentions it.
    #[test]
    fn the_starter_file_covers_every_configuration_key() {
        fn keys(v: &toml::Value, prefix: &str, out: &mut Vec<String>) {
            if let Some(table) = v.as_table() {
                for (k, val) in table {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    if val.is_table() {
                        keys(val, &path, out);
                    } else {
                        out.push(path);
                    }
                }
            }
        }

        // No `..Default::default()` anywhere below, on purpose: that is what
        // makes adding a config field a compile error here rather than a
        // silently unguarded setting.
        let everything = crate::config::Config {
            server: crate::config::Server {
                bind: "127.0.0.1:8787".parse().expect("literal"),
                public_url: "https://fleet.example".into(),
                collection_dir: Some("collection".into()),
                index: "i.db".into(),
                archive_dir: Some("archive".into()),
                scan_interval_minutes: 15,
            },
            auth: crate::config::Auth {
                mode: crate::config::AuthMode::Oidc,
                issuer: "https://idp.example".into(),
                client_id: "id".into(),
                client_secret: "secret".into(),
                groups_claim: "groups".into(),
                session_hours: 8,
                extra_scopes: vec!["groups".into()],
                ca_bundle: Some("ca.pem".into()),
                grants: vec![crate::config::Grant {
                    group: "g".into(),
                    role: crate::config::Role::Admin,
                    tags: Default::default(),
                }],
            },
            metrics: crate::config::Metrics {
                enabled: true,
                token: "t".into(),
            },
            log: crate::config::Log {
                format: crate::config::LogFormat::Text,
                level: "info".into(),
                file: Some("fleet.log".into()),
            },
        };

        let defaults: toml::Value =
            toml::from_str(&toml::to_string(&everything).expect("Config serialises"))
                .expect("valid TOML");
        let starter: toml::Value = toml::from_str(&Config::starter()).expect("starter is TOML");

        let mut want = Vec::new();
        keys(&defaults, "", &mut want);
        let mut have = Vec::new();
        keys(&starter, "", &mut have);

        // A commented-out optional key is still documented, so check the text
        // as well as the parsed table.
        let text = Config::starter();
        let undocumented: Vec<&String> = want
            .iter()
            .filter(|k| {
                let leaf = k.rsplit('.').next().unwrap();
                !have.contains(k) && !text.contains(&format!("{leaf} ="))
            })
            .collect();
        assert!(
            undocumented.is_empty(),
            "configuration keys missing from `init-config`, so missing from the \
             reference too: {undocumented:?}"
        );
    }

    /// The page is what a wiki renders, so its own internal links have to work.
    #[test]
    fn the_command_index_links_resolve() {
        let page = markdown(root());
        let headings: Vec<String> = page
            .lines()
            .filter(|l| l.starts_with('#'))
            .map(|l| anchor(l.trim_start_matches('#').trim()))
            .collect();
        let mut broken = Vec::new();
        for line in page.lines().filter(|l| l.contains("](#")) {
            for part in line.split("](#").skip(1) {
                let target = part.split(')').next().unwrap_or_default();
                if !headings.contains(&target.to_string()) {
                    broken.push(target.to_string());
                }
            }
        }
        assert!(
            broken.is_empty(),
            "dead anchors in the reference: {broken:?}"
        );
    }
}
