//! Argument parsing, by hand.
//!
//! No `clap`: the workspace has never pulled one, the grammar below is three
//! subcommands and four flags, and a satellite binary is the last place to add
//! a derive-macro dependency for that (low-power rule 5).
//!
//! One rule shapes the whole grammar: **no secret may appear in argv**
//! (invariant 5). There is deliberately no `--code` and no `--token`; the
//! pairing code is read from the terminal, where the owner is already standing.

use anyhow::{Result, bail};

use crate::pairing::NODE_CLASSES;

pub const USAGE: &str = "\
jarvis-agent — a paired Jarvis node (display, voice, or both)

USAGE:
    jarvis-agent pair --server <url> --name <name> [--class <class>]
    jarvis-agent run [--node]
    jarvis-agent reset

COMMANDS:
    pair     Pair with a daemon. Prompts for the one-time code the owner reads
             out; the code is never taken from an argument or the environment.
    run      Connect using the stored credentials and stay connected.
    reset    Forget this node's credentials (after revoking it in the shell).

OPTIONS:
    --server <url>   Daemon base URL, e.g. https://jarvis.lan:8741
    --name <name>    How this node appears in the owner's device list
    --class <class>  display-node | voice-node | room-node  (default: room-node)
    --node           Accepted on `run` for symmetry with the docs; the role is
                     decided by the class the server assigned at pairing, never
                     by a flag on the node itself.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Pair {
        server: String,
        name: String,
        class: String,
    },
    Run,
    Reset,
    Help,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(Command::Help);
    };

    match command.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "reset" => Ok(Command::Reset),
        "run" => {
            for arg in args {
                match arg.as_str() {
                    // Accepted and intentionally inert — see USAGE.
                    "--node" => {}
                    other => bail!("unexpected argument to `run`: {other}"),
                }
            }
            Ok(Command::Run)
        }
        "pair" => {
            let mut server = None;
            let mut name = None;
            let mut class = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--server" => server = Some(value(&mut args, "--server")?),
                    "--name" => name = Some(value(&mut args, "--name")?),
                    "--class" => class = Some(value(&mut args, "--class")?),
                    "--code" => bail!(
                        "the pairing code is not accepted as an argument — it would land in \
                         your shell history and in `ps` output. Run `pair` without it and \
                         type the code when prompted."
                    ),
                    other => bail!("unexpected argument to `pair`: {other}"),
                }
            }
            let class = class.unwrap_or_else(|| "room-node".to_owned());
            if !NODE_CLASSES.contains(&class.as_str()) {
                bail!("--class must be one of {}", NODE_CLASSES.join(", "));
            }
            Ok(Command::Pair {
                server: server.ok_or_else(|| anyhow::anyhow!("pair requires --server"))?,
                name: name.ok_or_else(|| anyhow::anyhow!("pair requires --name"))?,
                class,
            })
        }
        other => bail!("unknown command: {other}\n\n{USAGE}"),
    }
}

fn value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String> {
    match args.next() {
        Some(value) if !value.starts_with("--") => Ok(value),
        _ => bail!("{flag} requires a value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parses_a_pairing_invocation() {
        let command = parse_args(&[
            "pair",
            "--server",
            "https://jarvis.lan:8741",
            "--name",
            "kitchen",
            "--class",
            "room-node",
        ])
        .expect("parses");
        assert_eq!(
            command,
            Command::Pair {
                server: "https://jarvis.lan:8741".into(),
                name: "kitchen".into(),
                class: "room-node".into(),
            }
        );
    }

    #[test]
    fn class_defaults_to_room_node_and_owner_ui_is_not_on_the_menu() {
        let Command::Pair { class, .. } =
            parse_args(&["pair", "--server", "http://x", "--name", "n"]).expect("parses")
        else {
            panic!("expected pair");
        };
        assert_eq!(class, "room-node");

        // A node may not even ask to be the owner's client (docs/05 §6.3).
        assert!(
            parse_args(&[
                "pair", "--server", "http://x", "--name", "n", "--class", "owner-ui"
            ])
            .is_err()
        );
    }

    /// Invariant 5, enforced at the grammar: there is no way to put the code in
    /// argv, and trying is an error that explains itself.
    #[test]
    fn the_pairing_code_cannot_be_passed_as_an_argument() {
        let error = parse_args(&[
            "pair", "--server", "http://x", "--name", "n", "--code", "123456",
        ])
        .expect_err("must refuse");
        assert!(
            error.to_string().contains("not accepted as an argument"),
            "{error}"
        );
    }

    #[test]
    fn run_accepts_the_node_flag_and_rejects_anything_else() {
        assert_eq!(parse_args(&["run"]).expect("parses"), Command::Run);
        assert_eq!(
            parse_args(&["run", "--node"]).expect("parses"),
            Command::Run
        );
        assert!(parse_args(&["run", "--class", "voice-node"]).is_err());
    }

    #[test]
    fn a_flag_that_swallowed_the_next_flag_is_an_error() {
        // `--name --class` must not silently name the node "--class".
        assert!(parse_args(&["pair", "--server", "http://x", "--name", "--class"]).is_err());
    }

    #[test]
    fn no_arguments_prints_usage_rather_than_guessing() {
        assert_eq!(parse_args(&[]).expect("parses"), Command::Help);
    }
}
