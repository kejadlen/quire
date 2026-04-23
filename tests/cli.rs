use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;

fn cmd() -> Command {
    Command::from(cargo_bin_cmd!("quire"))
}

#[test]
fn shows_help() {
    cmd().arg("--help").assert().success();
}

#[test]
fn shows_version() {
    cmd().arg("--version").assert().success();
}
