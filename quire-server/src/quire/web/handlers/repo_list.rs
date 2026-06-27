//! Handler for the repository list (home) page.

use axum::extract::State;
use axum::response::Response;

use super::super::templates::{self, ListedRepo};
use super::git::RepoView;
use super::render;
use crate::Quire;

pub async fn repo_list(State(quire): State<Quire>) -> Response {
    let repos: Vec<ListedRepo> = match quire.repos() {
        Ok(iter) => iter
            .map(|repo| {
                let name = repo.name().to_string().trim_end_matches(".git").to_string();
                let description = if repo.exists() {
                    let reader = RepoView::new(&repo);
                    reader.run(&["log", "-1", "--format=%s"])
                } else {
                    None
                };
                ListedRepo { name, description }
            })
            .collect(),
        Err(e) => {
            tracing::error!(
                error = &e as &(dyn std::error::Error + 'static),
                "failed to list repos"
            );
            vec![]
        }
    };

    render(templates::repo_list(&repos))
}
