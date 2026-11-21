use crate::{
    bot_templates::MESSAGE_LIMIT,
    templates::{RenderedTemplate, TemplateEngine},
};
use anyhow::{Context, ensure};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const PRYDWEN: &str = "https://www.prydwen.gg/zenless/codes";
const HOYO_CODES: &str = "https://hoyo-codes.seria.moe/codes?game=nap";

pub struct Listing {
    pub source: &'static str,
    pub codes: Vec<PromoCode>,
}

#[derive(Debug, Serialize)]
pub struct PromoCode {
    pub code: String,
    pub rewards: Vec<String>,
}

impl Listing {
    pub fn messages(&self, templates: &TemplateEngine) -> anyhow::Result<Vec<RenderedTemplate>> {
        let rendered = templates.render(
            "promocodes",
            &serde_json::json!({
                "source": self.source, "codes": self.codes,
            }),
        )?;
        if rendered.text.chars().count() <= MESSAGE_LIMIT {
            return Ok(vec![rendered]);
        }
        // The template separates the heading and each complete code block with a blank line.
        let (heading, entries) = rendered
            .text
            .split_once("\n\n")
            .context("missing promo code blocks")?;
        let mut pages = Vec::new();
        let mut page = heading.to_owned();
        for entry in entries.split("\n\n") {
            if page.chars().count() + 2 + entry.chars().count() > MESSAGE_LIMIT && page != heading {
                pages.push(RenderedTemplate {
                    text: std::mem::replace(&mut page, heading.to_owned()),
                    format: rendered.format,
                });
            }
            page.push_str("\n\n");
            page.push_str(entry);
        }
        pages.push(RenderedTemplate {
            text: page,
            format: rendered.format,
        });
        Ok(pages)
    }
}

pub async fn fetch(client: &reqwest::Client) -> anyhow::Result<Listing> {
    let primary = async {
        let html = client
            .get(PRYDWEN)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_prydwen(&html)
    }
    .await;
    let (source, mut codes) = match primary {
        Ok(codes) => ("Prydwen", codes),
        Err(error) => {
            log::warn!("Prydwen promo codes unavailable: {error:#}");
            let response: ApiResponse = client
                .get(HOYO_CODES)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
                .context("invalid HoYo Codes response")?;
            ("HoYo Codes", response.active_codes()?)
        }
    };
    let mut seen = HashSet::new();
    codes.retain(|entry| valid_code(&entry.code) && seen.insert(entry.code.to_ascii_uppercase()));
    Ok(Listing { source, codes })
}

fn valid_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Deserialize)]
struct ApiResponse {
    game: String,
    codes: Vec<ApiCode>,
}

#[derive(Deserialize)]
struct ApiCode {
    game: String,
    status: String,
    code: String,
    #[serde(default)]
    rewards: Option<String>,
}

impl ApiResponse {
    fn active_codes(self) -> anyhow::Result<Vec<PromoCode>> {
        ensure!(self.game == "nap", "unexpected HoYo Codes game");
        Ok(self
            .codes
            .into_iter()
            .filter(|entry| entry.game == "nap" && entry.status == "OK")
            .map(|entry| {
                let text = entry.rewards.as_deref().unwrap_or_default();
                // Some entries are prose lists, including non-breaking spaces after commas.
                let text = text.replace('\u{a0}', " ");
                let rewards = text
                    .split([';', '\n'])
                    .flat_map(|part| part.split(", "))
                    .map(|part| part.trim().trim_start_matches("and "))
                    .filter(|part| !part.is_empty())
                    .map(format_reward)
                    .collect();
                PromoCode {
                    code: entry.code,
                    rewards,
                }
            })
            .collect())
    }
}

fn format_reward(text: &str) -> String {
    match text.rsplit_once(['×', '*']) {
        Some((name, quantity)) => format!("{} x {}", name.trim(), quantity.trim()),
        None => text.trim().to_owned(),
    }
}

fn parse_prydwen(html: &str) -> anyhow::Result<Vec<PromoCode>> {
    let document = Html::parse_document(html);
    let cards = Selector::parse("#active-codes article[data-code], #livestream-codes:not([data-program-state=expired]) article[data-code]").unwrap();
    let empty = Selector::parse("#active-codes [data-active-empty]:not([hidden])").unwrap();
    let reward = Selector::parse(".code-reward").unwrap();
    let mut cards = document.select(&cards).peekable();
    ensure!(
        cards.peek().is_some() || document.select(&empty).next().is_some(),
        "unrecognized Prydwen codes page"
    );
    let mut codes = Vec::new();
    let now = chrono::Utc::now();
    for card in cards {
        if card.attr("data-state") == Some("expired")
            || card.attr("data-manually-expired") == Some("true")
        {
            continue;
        }
        ensure!(
            card.attr("data-state") == Some("active"),
            "unknown Prydwen code status"
        );
        if let Some(expiry) = card
            .attr("data-expires-at")
            .filter(|value| !value.is_empty())
            && chrono::DateTime::parse_from_rfc3339(expiry)? <= now
        {
            continue;
        }
        let code = card.attr("data-code").context("missing Prydwen code")?;
        ensure!(valid_code(code), "unexpected Prydwen code format");
        let rewards = card
            .select(&reward)
            .map(|element| format_reward(&element.text().collect::<String>()))
            .filter(|reward| !reward.is_empty())
            .collect();
        codes.push(PromoCode {
            code: code.to_owned(),
            rewards,
        });
    }
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_active_and_livestream_sections_and_skips_expired_codes() {
        let html = "<section id='active-codes'><article data-code='MixedCase' data-state='active'>
            <span class='code-reward'><span>Polychrome<!-- --> <strong>×<!-- -->60</strong></span></span>
            <span class='code-reward'>Denny <strong>×2,222</strong></span></article></section>
            <section id='livestream-codes'><article data-code='STREAM' data-state='active'></article>
            <article data-code='OLDSTREAM' data-state='expired'></article>
            <article data-code='OVERDUE' data-state='active' data-expires-at='2020-01-01T00:00:00Z'></article></section>
            <section id='expired-codes'><code>OLD</code></section>";
        let codes = parse_prydwen(html).unwrap();
        assert_eq!(
            codes
                .iter()
                .map(|entry| entry.code.as_str())
                .collect::<Vec<_>>(),
            ["MixedCase", "STREAM"]
        );
        assert_eq!(codes[0].rewards, ["Polychrome x 60", "Denny x 2,222"]);
        assert!(codes[1].rewards.is_empty());
    }

    #[test]
    fn distinguishes_empty_list_from_unavailable_page() {
        let empty = "<section id='active-codes'><p data-active-empty>No active codes are listed.</p></section>";
        assert!(parse_prydwen(empty).unwrap().is_empty());
        assert!(parse_prydwen("<title>Just a moment...</title>").is_err());
    }

    #[test]
    fn api_returns_only_active_zzz_codes() {
        let response: ApiResponse = serde_json::from_value(serde_json::json!({
            "game": "nap",
            "codes": [
                {"game":"nap", "status":"OK", "code":"MixedCase", "rewards":"Polychrome*20;Denny*2,222"},
                {"game":"nap", "status":"OK", "code":"EMPTY"},
                {"game":"nap", "status":"OK", "code":"PROSE", "rewards":"50 polychrome, two logs, and one module"},
                {"game":"nap", "status":"NOT_OK", "code":"EXPIRED"},
                {"game":"hkrpg", "status":"OK", "code":"OTHERGAME"}
            ]
        }))
        .unwrap();
        let codes = response.active_codes().unwrap();
        assert_eq!(codes.len(), 3);
        assert_eq!(codes[0].code, "MixedCase");
        assert_eq!(codes[0].rewards, ["Polychrome x 20", "Denny x 2,222"]);
        assert!(codes[1].rewards.is_empty());
        assert_eq!(
            codes[2].rewards,
            ["50 polychrome", "two logs", "one module"]
        );
    }

    #[test]
    fn paginates_complete_code_blocks_and_escapes_rewards() {
        let codes = (0..100)
            .map(|i| PromoCode {
                code: format!("CODE{i}"),
                rewards: vec!["A & B x 60".into(), "Denny x 2,222".into()],
            })
            .collect();
        let listing = Listing {
            source: "Prydwen",
            codes,
        };
        let pages = listing.messages(&TemplateEngine::new().unwrap()).unwrap();
        assert!(pages.len() > 1);
        let mut i = 0;
        for page in pages {
            assert!(page.text.chars().count() <= MESSAGE_LIMIT);
            let mut blocks = page.text.split("\n\n");
            assert_eq!(blocks.next(), Some("<b>Промокоды ZZZ</b> · Prydwen"));
            for block in blocks {
                assert_eq!(
                    block,
                    format!(
                        "<a href=\"https://zenless.hoyoverse.com/redemption?code=CODE{i}\">CODE{i}</a>\n• A &amp; B x 60\n• Denny x 2,222"
                    )
                );
                i += 1;
            }
        }
        assert_eq!(i, 100);
    }
}
