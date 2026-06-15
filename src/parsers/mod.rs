use crate::cookies::CookieJar;
use crate::error::FacebedResult;
use crate::fetch::Fetcher;
use std::sync::Arc;

pub mod json_post;
pub mod photocom;
pub mod reels;
pub mod single_photo;
pub mod stories;
pub mod util;
pub mod video_watch;

#[derive(Debug, Clone)]
pub struct ParsedPost {
    pub author_name: String,
    pub text: String,
    /// Let trusted FB-authored Markdown render in Discord embed descriptions.
    pub allow_discord_markdown: bool,
    pub image_links: Vec<String>,
    pub url: String,
    pub date: i64,
    pub likes: String,
    pub comments: String,
    pub shares: String,
    pub video_links: Vec<String>,
    /// Preview/thumbnail for a video. Used as the embed image when the video
    /// itself is too big to inline (Discord's ~25 MB media proxy limit).
    pub thumbnail: Option<String>,
}

pub struct ParserCtx {
    pub fetcher: Arc<Fetcher>,
    pub cookies: Arc<CookieJar>,
    pub banned_users: Vec<String>,
}

impl ParserCtx {
    pub fn is_banned(&self, author_id: &str) -> bool {
        self.banned_users.iter().any(|b| b == author_id)
    }
}

pub fn banned_post(url: &str) -> ParsedPost {
    ParsedPost {
        author_name: "Banned".into(),
        text: "This user is banned by the operators of this embed server".into(),
        allow_discord_markdown: false,
        image_links: Vec::new(),
        url: url.to_owned(),
        date: -1,
        likes: "null".into(),
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
