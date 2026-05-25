use once_cell::sync::Lazy;
use regex::Regex;

/// Crawlers/bots whose User-Agent should receive the embed instead of a redirect.
/// Pattern is a single case-insensitive regex.
static CRAWLER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(discordbot|slackbot|telegrambot|twitterbot|facebookexternalhit|whatsapp|skypeuripreview|redditbot|linkedinbot|googlebot|bingbot|yandexbot|duckduckbot|applebot|petalbot|gptbot|chatgpt-user|claudebot|bytespider|crawler|spider|preview|embedly|iframely|tumblrcrawler|developers\.google\.com/\+/web/snippet|nuzzel|outbrain|pinterestbot|qwantify|vkshare|w3c_validator|yahoo|baiduspider|coccocbot|sogou|seznambot|exabot|aolbuild|mediapartners-google|adsbot-google)\b",
    )
    .unwrap()
});

pub fn is_crawler(ua: &str) -> bool {
    !ua.is_empty() && CRAWLER_RE.is_match(ua)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_crawlers() {
        assert!(is_crawler(
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)"
        ));
        assert!(is_crawler("TelegramBot (like TwitterBot)"));
        assert!(is_crawler("facebookexternalhit/1.1"));
        assert!(is_crawler(
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)"
        ));
    }

    #[test]
    fn ignores_real_browsers() {
        assert!(!is_crawler(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/132.0.0.0"
        ));
        assert!(!is_crawler(""));
    }
}
