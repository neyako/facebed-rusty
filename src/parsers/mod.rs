use crate::config::Config;
use crate::cookies::CookieJar;
use crate::error::FacebedResult;
use crate::fetch::Fetcher;
use std::sync::Arc;

pub mod comment;
pub mod json_post;
pub mod photocom;
pub mod reels;
pub mod single_photo;
pub mod stories;
pub mod util;
pub mod video_watch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostContext {
    pub author_name: String,
    pub text: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    Like,
    Love,
    Care,
    Haha,
    Wow,
    Sad,
    Angry,
}

impl ReactionKind {
    pub fn emoji(self) -> &'static str {
        match self {
            Self::Like => "👍",
            Self::Love => "❤️",
            Self::Care => "🤗",
            Self::Haha => "😂",
            Self::Wow => "😮",
            Self::Sad => "😢",
            Self::Angry => "😡",
        }
    }

    pub fn from_id_or_name(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "1635855486666999" | "like" => Some(Self::Like),
            "1678524932434102" | "love" => Some(Self::Love),
            "613557422527858" | "care" => Some(Self::Care),
            "115940658764963" | "haha" => Some(Self::Haha),
            "478547315650144" | "wow" => Some(Self::Wow),
            "908563459236466" | "sad" => Some(Self::Sad),
            "444813342392137" | "angry" => Some(Self::Angry),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedPost {
    pub author_name: String,
    pub author_id: Option<String>,
    pub author_handle: Option<String>,
    pub author_avatar_url: Option<String>,
    pub context: Option<PostContext>,
    pub text: String,
    /// Let trusted FB-authored Markdown render in Discord embed descriptions.
    pub allow_discord_markdown: bool,
    pub image_links: Vec<String>,
    pub url: String,
    pub date: i64,
    pub likes: String,
    pub top_reaction_ids: Vec<ReactionKind>,
    pub comments: String,
    pub shares: String,
    pub video_links: Vec<String>,
    /// Preview/thumbnail for a video. Used as the embed image when the video
    /// itself is too big to inline (Discord's ~25 MB media proxy limit).
    pub thumbnail: Option<String>,
}

pub struct ParserCtx {
    pub fetcher: Arc<Fetcher>,
    pub cookies: Arc<arc_swap::ArcSwap<CookieJar>>,
    pub config: Arc<arc_swap::ArcSwap<Config>>,
}

impl ParserCtx {
    pub fn is_banned(&self, author_id: &str) -> bool {
        self.config
            .load()
            .banned_users
            .iter()
            .any(|b| b == author_id)
    }
}

pub async fn resolve_facebook_author_handle(ctx: &ParserCtx, mut post: ParsedPost) -> ParsedPost {
    if post.author_handle.is_none() {
        if let Some(author_id) = post.author_id.as_deref() {
            // Numeric IDs are already valid Activity usernames. A profile HEAD
            // cannot improve them and only adds latency to the crawler path.
            if author_id.bytes().all(|byte| byte.is_ascii_digit()) {
                return post;
            }
            post.author_handle = ctx.fetcher.resolve_profile_handle(author_id).await;
        }
    }
    post
}

pub fn banned_post(url: &str) -> ParsedPost {
    ParsedPost {
        author_name: "Banned".into(),
        author_id: None,
        author_handle: None,
        author_avatar_url: None,
        context: None,
        text: "This user is banned by the operators of this embed server".into(),
        allow_discord_markdown: false,
        image_links: Vec::new(),
        url: url.to_owned(),
        date: -1,
        likes: "null".into(),
        top_reaction_ids: Vec::new(),
        comments: "null".into(),
        shares: "null".into(),
        video_links: Vec::new(),
        thumbnail: None,
    }
}

#[async_trait::async_trait]
pub trait Parser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banned_reload_takes_effect_after_swap() {
        let swap = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));

        assert!(!swap.load().banned_users.iter().any(|b| b == "100012345"));

        let cfg = Config {
            banned_users: vec!["100012345".to_string()],
            ..Config::default()
        };
        swap.store(Arc::new(cfg));

        assert!(
            swap.load().banned_users.iter().any(|b| b == "100012345"),
            "swapping config must make the new banned id visible"
        );
    }
}
