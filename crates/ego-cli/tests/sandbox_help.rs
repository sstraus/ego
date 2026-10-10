use ego_cli::cli::{self, Invocation};
use ego_core::permit::Confinement;

// Catches: accepted adaptive confinement omitted from sandbox help.
#[test]
fn sandbox_help_lists_every_accepted_parser_value() {
    for command in ["run", "resume", "acp", "a2a"] {
        let Invocation::Answered(help) =
            cli::parse(["ego", command, "--help"].map(str::to_owned).to_vec()).unwrap()
        else {
            panic!("help must be answered for {command}");
        };
        let sandbox_help = help
            .lines()
            .skip_while(|line| !line.contains("--sandbox"))
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        for spelling in ["ro", "workspace", "adaptive", "off"] {
            let _: Confinement = spelling.parse().expect("accepted confinement");
            assert!(
                sandbox_help.contains(spelling),
                "{command} sandbox help omits accepted {spelling}: {sandbox_help}"
            );
        }
    }
}
