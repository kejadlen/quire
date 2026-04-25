use miette::{IntoDiagnostic, Result};

use quire::Config;

pub async fn run(config: &Config) -> Result<()> {
    let entries = fs_err::read_dir(&config.repos_dir).into_diagnostic()?;

    let mut repos: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let Ok(relative) = path.strip_prefix(&config.repos_dir) else {
            continue;
        };
        let name = relative.to_string_lossy();

        // Top-level .git directory.
        if name.ends_with(".git") {
            repos.push(name.to_string());
            continue;
        }

        // Group directory — collect .git children.
        let Ok(children) = fs_err::read_dir(&path) else {
            continue;
        };
        for child in children {
            let child = child.into_diagnostic()?;
            let child_name = child.file_name();
            let child_name = child_name.to_string_lossy();
            if child_name.ends_with(".git") && child.path().is_dir() {
                let full = format!("{}/{}", name, child_name);
                repos.push(full);
            }
        }
    }

    repos.sort();
    for repo in &repos {
        println!("{repo}");
    }

    Ok(())
}
