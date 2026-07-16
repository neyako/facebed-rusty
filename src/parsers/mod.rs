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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banned_reload_takes_effect_after_swap() {
        let swap = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));

        assert!(!swap.load().banned_users.iter().any(|b| b == "100012345"));

        let mut cfg = Config::default();
        cfg.banned_users = vec!["100012345".to_string()];
        swap.store(Arc::new(cfg));

        assert!(
            swap.load().banned_users.iter().any(|b| b == "100012345"),
            "swapping config must make the new banned id visible"
        );
    }
}
