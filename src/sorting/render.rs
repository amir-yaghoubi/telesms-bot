use crate::db::Classification;
use crate::otp::looks_like_otp;
use crate::sorting::config::{SortSpec, SortingConfig};
use crate::status::html_escape;

pub struct InboundPost {
    pub html: String,
    pub teach_button: bool,
}

pub fn render_inbound(
    c: Option<&Classification>,
    otp_code: Option<&str>,
    number_line: Option<&str>,
    body: &str,
    cfg: &SortingConfig,
) -> InboundPost {
    let mut lines = Vec::new();
    if let Some(heading) = heading(c, otp_code.is_some(), cfg) {
        lines.push(heading);
    }
    if let Some(code) = otp_code {
        lines.push(format!("<code>{}</code>", html_escape(code)));
    }
    if let Some(number) = number_line {
        lines.push(html_escape(number));
    }
    lines.push(html_escape(body));
    InboundPost {
        html: lines.join("\n"),
        teach_button: otp_code.is_none() && looks_like_otp(body),
    }
}

fn heading(c: Option<&Classification>, has_code: bool, cfg: &SortingConfig) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(c) = c {
        if let Some(spec) = cfg.sorts.get(&c.category) {
            parts.push(label(spec));
        }
        if c.tags.iter().any(|t| t == "scam") {
            if let Some(spec) = cfg.sorts.get("scam") {
                parts.push(label(spec));
            }
        }
    }
    if parts.is_empty() && has_code {
        if let Some(spec) = cfg.sorts.get("otp") {
            parts.push(label(spec));
        }
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

fn label(spec: &SortSpec) -> String {
    format!("{} <b>{}</b>", spec.emoji, html_escape(&spec.title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SortingConfig {
        SortingConfig::load(None).unwrap()
    }

    fn cls(category: &str, confidence: f64, tags: &[&str]) -> Classification {
        Classification {
            category: category.to_string(),
            confidence,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn renders_heading_code_number_body() {
        let c = cls("promotional", 0.9, &["scam"]);
        let post = render_inbound(
            Some(&c),
            Some("402332"),
            Some("+989981600237"),
            "a < b & c",
            &cfg(),
        );
        assert_eq!(
            post.html,
            "🟨 <b>تبلیغاتی</b>  🟥 <b>کلاهبرداری/فیشینگ</b>\n<code>402332</code>\n+989981600237\na &lt; b &amp; c"
        );
        assert!(!post.teach_button);
    }

    #[test]
    fn plain_post_when_nothing_known() {
        let post = render_inbound(None, None, Some("+98912"), "hi", &cfg());
        assert_eq!(post.html, "+98912\nhi");
        assert!(!post.teach_button);
    }

    #[test]
    fn teach_button_on_unlearned_otp() {
        let post = render_inbound(None, None, None, "کد ورود: 5388", &cfg());
        assert!(post.teach_button);
    }

    #[test]
    fn otp_heading_when_only_code_is_known() {
        let post = render_inbound(None, Some("402332"), None, "hi", &cfg());
        assert_eq!(post.html, "🟩 <b>کد یکبارمصرف</b>\n<code>402332</code>\nhi");
    }

    #[test]
    fn category_without_sort_entry_omits_heading_part() {
        let c = cls("unknown", 0.9, &["scam"]);
        let post = render_inbound(Some(&c), None, None, "hi", &cfg());
        assert_eq!(post.html, "🟥 <b>کلاهبرداری/فیشینگ</b>\nhi");
    }
}
