use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=QUIRE_VERSION");
    let version = std::env::var("QUIRE_VERSION").unwrap_or_else(|_| {
        let date = cmd("date", &["-u", "+%Y.%m.%d"]);
        let change = cmd(
            "jj",
            &[
                "log",
                "--revisions",
                "@",
                "--no-graph",
                "--template",
                "change_id.short()",
            ],
        );
        format!("{date}+{change}-dev")
    });
    println!("cargo:rustc-env=QUIRE_VERSION={version}");
}

fn cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
