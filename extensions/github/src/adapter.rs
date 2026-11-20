use crate::{Result, fail, schema::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
const REPO: &str = "id databaseId nameWithOwner description url homepageUrl isPrivate isArchived primaryLanguage { name } repositoryTopics(first:20) { nodes { topic { name } } } owner { __typename id login url avatarUrl ... on User { databaseId name } ... on Organization { databaseId name } }";
pub struct Adapter {
    client: reqwest::Client,
    token: String,
    url: String,
}
impl Adapter {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| fail("HTTP client unavailable"))?,
            token,
            url: std::env::var("INKCRE_GITHUB_GRAPHQL_URL")
                .unwrap_or_else(|_| "https://api.github.com/graphql".into()),
        })
    }
    async fn execute(&self, query: String, variables: Value) -> Result<Value> {
        let mut response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("User-Agent", "InKCre-GitHub-Extension")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&json!({"query":query,"variables":variables}))
            .send()
            .await
            .map_err(|_| fail("GitHub request failed"))?
            .error_for_status()
            .map_err(|_| fail("GitHub request rejected"))?;
        let mut bytes = Vec::new();
        while let Some(part) = response
            .chunk()
            .await
            .map_err(|_| fail("GitHub response failed"))?
        {
            if bytes.len() + part.len() > 16 * 1024 * 1024 {
                return Err(fail("GitHub response exceeded limit"));
            }
            bytes.extend(part)
        }
        let payload: Value =
            serde_json::from_slice(&bytes).map_err(|_| fail("Invalid GitHub response"))?;
        if payload
            .get("errors")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
        {
            return Err(fail("GitHub returned GraphQL errors"));
        }
        payload
            .get("data")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| fail("GitHub data missing"))
    }
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let mut repositories = BTreeMap::new();
        let mut stars = BTreeSet::new();
        let mut account_value: Option<Account> = None;
        let mut cursor = Value::Null;
        let mut seen = BTreeSet::new();
        loop {
            let data=self.execute(format!("query GitHubStars($cursor:String) {{ viewer {{ id databaseId login name url avatarUrl starredRepositories(first:50,after:$cursor) {{ nodes {{ {REPO} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"),json!({"cursor":cursor})).await?;
            let viewer = &data["viewer"];
            let account = account(viewer, true)?;
            if account_value
                .as_ref()
                .is_some_and(|a| a.node_id != account.node_id)
            {
                return Err(fail("GitHub viewer changed during pagination"));
            }
            account_value = Some(account);
            let connection = &viewer["starredRepositories"];
            for node in nodes(connection)? {
                let fact = repository(node)?;
                stars.insert(fact.repository.node_id.clone());
                repositories.insert(fact.repository.node_id.clone(), fact);
            }
            if !next(connection, &mut cursor, &mut seen)? {
                break;
            }
        }
        let mut lists = Vec::new();
        cursor = Value::Null;
        seen.clear();
        loop {
            let data=self.execute("query GitHubLists($cursor:String){ viewer { lists(first:100,after:$cursor){ nodes { id name description slug isPrivate items(first:100){ nodes { ... on Repository { id } } pageInfo { hasNextPage endCursor } } } pageInfo { hasNextPage endCursor } } } }".into(),json!({"cursor":cursor})).await?;
            let connection = &data["viewer"]["lists"];
            for node in nodes(connection)? {
                let list = List {
                    node_id: string(node, "id")?,
                    name: string(node, "name")?,
                    description: optional(node, "description")?,
                    slug: string(node, "slug")?,
                    is_private: boolean(node, "isPrivate")?,
                };
                let mut items = BTreeSet::new();
                let mut item_cursor = Value::Null;
                let mut item_seen = BTreeSet::new();
                let mut connection = node["items"].clone();
                loop {
                    for item in nodes(&connection)? {
                        items.insert(string(item, "id")?);
                    }
                    if !next(&connection, &mut item_cursor, &mut item_seen)? {
                        break;
                    }
                    let page=self.execute("query GitHubListItems($id:ID!,$cursor:String){ node(id:$id){ ... on UserList { items(first:100,after:$cursor){ nodes { ... on Repository { id } } pageInfo { hasNextPage endCursor } } } } }".into(),json!({"id":list.node_id,"cursor":item_cursor})).await?;
                    connection = page["node"]["items"].clone();
                }
                lists.push(ListFact {
                    list,
                    repositories: items.into_iter().collect(),
                });
            }
            if !next(connection, &mut cursor, &mut seen)? {
                break;
            }
        }
        let missing: BTreeSet<_> = lists
            .iter()
            .flat_map(|l| l.repositories.iter())
            .filter(|id| !repositories.contains_key(*id))
            .cloned()
            .collect();
        for ids in missing.into_iter().collect::<Vec<_>>().chunks(50) {
            let data=self.execute(format!("query GitHubRepositories($ids:[ID!]!) {{ nodes(ids:$ids) {{ ... on Repository {{ {REPO} }} }} }}"),json!({"ids":ids})).await?;
            for node in data["nodes"]
                .as_array()
                .ok_or_else(|| fail("Repository nodes missing"))?
            {
                let fact = repository(node)?;
                repositories.insert(fact.repository.node_id.clone(), fact);
            }
        }
        if lists
            .iter()
            .flat_map(|l| l.repositories.iter())
            .any(|id| !repositories.contains_key(id))
        {
            return Err(fail("List repository unresolved"));
        }
        Ok(Snapshot {
            account: account_value.ok_or_else(|| fail("Account missing"))?,
            repositories: repositories.into_values().collect(),
            stars: stars.into_iter().collect(),
            lists,
        })
    }
}
fn nodes(connection: &Value) -> Result<&Vec<Value>> {
    connection["nodes"]
        .as_array()
        .ok_or_else(|| fail("Invalid GitHub connection"))
}
fn next(connection: &Value, cursor: &mut Value, seen: &mut BTreeSet<String>) -> Result<bool> {
    let page = &connection["pageInfo"];
    let more = boolean(page, "hasNextPage")?;
    if more {
        let key = string(page, "endCursor")?;
        if !seen.insert(key.clone()) {
            return Err(fail("GitHub pagination did not advance"));
        }
        *cursor = json!(key);
    }
    Ok(more)
}
fn string(v: &Value, k: &str) -> Result<String> {
    v[k].as_str()
        .map(str::to_owned)
        .ok_or_else(|| fail("GitHub required string missing"))
}
fn optional(v: &Value, k: &str) -> Result<Option<String>> {
    if v[k].is_null() {
        Ok(None)
    } else {
        string(v, k).map(Some)
    }
}
fn boolean(v: &Value, k: &str) -> Result<bool> {
    v[k].as_bool().ok_or_else(|| fail("GitHub boolean missing"))
}
fn account(v: &Value, viewer: bool) -> Result<Account> {
    let kind = if viewer || v["__typename"] == "User" {
        "user"
    } else if v["__typename"] == "Organization" {
        "organization"
    } else {
        return Err(fail("Unsupported GitHub account"));
    };
    Ok(Account {
        node_id: string(v, "id")?,
        database_id: v["databaseId"].as_i64(),
        kind: kind.into(),
        login: string(v, "login")?,
        name: optional(v, "name")?,
        url: string(v, "url")?,
        avatar_url: optional(v, "avatarUrl")?,
    })
}
fn repository(v: &Value) -> Result<RepoFact> {
    let topics = nodes(&v["repositoryTopics"])?
        .iter()
        .filter_map(|v| v["topic"]["name"].as_str().map(str::to_owned))
        .collect();
    Ok(RepoFact {
        repository: Repository {
            node_id: string(v, "id")?,
            database_id: v["databaseId"].as_i64(),
            name_with_owner: string(v, "nameWithOwner")?,
            description: optional(v, "description")?,
            url: string(v, "url")?,
            homepage_url: optional(v, "homepageUrl")?,
            primary_language: v["primaryLanguage"]["name"].as_str().map(str::to_owned),
            topics,
            is_private: boolean(v, "isPrivate")?,
            is_archived: boolean(v, "isArchived")?,
        },
        owner: account(&v["owner"], false)?,
    })
}
