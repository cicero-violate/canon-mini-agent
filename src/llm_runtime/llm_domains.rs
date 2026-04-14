pub const CHATGPT_BASE: &str = "https://chatgpt.com/";
pub const CHATGPT_GG_BASE: &str = "https://chatgpt.com/gg/";
pub const CHATGPT_LEGACY_BASE: &str = "https://chat.openai.com/";
pub const GEMINI_BASE: &str = "https://gemini.google.com/";

pub fn is_chatgpt_gg_url(url: &str) -> bool {
    url.starts_with(CHATGPT_GG_BASE)
}

pub fn is_chatgpt_url(url: &str) -> bool {
    url.starts_with(CHATGPT_BASE) || url.starts_with(CHATGPT_LEGACY_BASE)
}

pub fn is_gemini_url(url: &str) -> bool {
    url.starts_with(GEMINI_BASE)
}
