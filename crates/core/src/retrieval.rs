//! Coordination across independent retrieval owners.

use serde::{Deserialize, Serialize};

use crate::{Core, Result, lexical, semantic};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Lexical,
    Semantic,
    #[default]
    Hybrid,
}

/// Each selected branch preserves its owner's result and diagnostic.
/// An absent branch was not requested.
pub struct Branches {
    pub lexical: Option<Result<lexical::LexicalResult>>,
    pub semantic: Option<Result<semantic::Retrieval>>,
}

impl Core {
    pub async fn retrieve(&self, query: &str, mode: Mode, limit: i64) -> Branches {
        let (lexical, semantic) = tokio::join!(
            async {
                if matches!(mode, Mode::Lexical | Mode::Hybrid) {
                    Some(
                        self.lexical_retrieve(lexical::LexicalRequest {
                            query: query.into(),
                            limit,
                            filters: None,
                        })
                        .await,
                    )
                } else {
                    None
                }
            },
            async {
                if matches!(mode, Mode::Semantic | Mode::Hybrid) {
                    Some(
                        self.semantic_retrieve(semantic::Request {
                            query: query.into(),
                            profile: None,
                            options: semantic::Options {
                                limit,
                                ..Default::default()
                            },
                        })
                        .await,
                    )
                } else {
                    None
                }
            },
        );
        Branches { lexical, semantic }
    }
}
