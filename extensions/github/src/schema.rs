use serde::{Deserialize, Serialize};
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub node_id: String,
    pub database_id: Option<i64>,
    pub kind: String,
    pub login: String,
    pub name: Option<String>,
    pub url: String,
    pub avatar_url: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub node_id: String,
    pub database_id: Option<i64>,
    pub name_with_owner: String,
    pub description: Option<String>,
    pub url: String,
    pub homepage_url: Option<String>,
    pub primary_language: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    pub is_private: bool,
    pub is_archived: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub node_id: String,
    pub name: String,
    pub description: Option<String>,
    pub slug: String,
    pub is_private: bool,
}
#[derive(Clone)]
pub struct RepoFact {
    pub repository: Repository,
    pub owner: Account,
}
#[derive(Clone)]
pub struct ListFact {
    pub list: List,
    pub repositories: Vec<String>,
}
pub struct Snapshot {
    pub account: Account,
    pub repositories: Vec<RepoFact>,
    pub stars: Vec<String>,
    pub lists: Vec<ListFact>,
}
pub const ACCOUNT: &str = "extensions.github.account.v1";
pub const REPOSITORY: &str = "extensions.github.repository.v1";
pub const LIST: &str = "extensions.github.list.v1";
pub const SOURCE: &str = "extensions.github.stars.Source";
pub fn schemas() -> serde_json::Value {
    serde_json::from_str(include_str!("schemas.json")).expect("static canonical schemas")
}
