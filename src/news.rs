use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(20 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(3 * 60);
const MAX_FEED_BYTES: u64 = 512 * 1024;
const MAX_HEADLINES: usize = 8;

#[derive(Debug, Clone)]
pub struct NewsData {
    pub headlines: Vec<String>,
    pub fetched_at: Instant,
}

pub type NewsUpdate = Result<NewsData, String>;

/// Fetch headlines independently of rendering and playback polling.
pub fn spawn_worker(feed_url: String) -> Receiver<NewsUpdate> {
    let (updates_tx, updates_rx) = mpsc::channel();
    thread::spawn(move || {
        if feed_url.trim().is_empty() {
            updates_tx
                .send(Err("Morning news feed is disabled".to_string()))
                .ok();
            return;
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        loop {
            let update = fetch_news(&agent, &feed_url);
            let delay = if update.is_ok() {
                REFRESH_INTERVAL
            } else {
                RETRY_INTERVAL
            };
            if updates_tx.send(update).is_err() {
                break;
            }
            thread::sleep(delay);
        }
    });
    updates_rx
}

fn fetch_news(agent: &ureq::Agent, feed_url: &str) -> NewsUpdate {
    let response = agent
        .get(feed_url)
        .set(
            "Accept",
            "application/rss+xml, application/xml, text/xml;q=0.9",
        )
        .set("User-Agent", "Orpheus/0.1 personal RSS reader")
        .call()
        .map_err(|error| format!("news request failed: {error}"))?;
    let mut xml = String::new();
    response
        .into_reader()
        .take(MAX_FEED_BYTES)
        .read_to_string(&mut xml)
        .map_err(|error| format!("could not read news response: {error}"))?;
    let headlines = parse_rss_headlines(&xml, MAX_HEADLINES);
    if headlines.is_empty() {
        return Err("news feed contained no headlines".to_string());
    }
    Ok(NewsData {
        headlines,
        fetched_at: Instant::now(),
    })
}

fn parse_rss_headlines(xml: &str, limit: usize) -> Vec<String> {
    let mut remaining = xml;
    let mut headlines = Vec::new();
    while let Some(item_start) = remaining.find("<item") {
        remaining = &remaining[item_start..];
        let Some(item_open_end) = remaining.find('>') else {
            break;
        };
        remaining = &remaining[item_open_end + 1..];
        let Some(item_end) = remaining.find("</item>") else {
            break;
        };
        let item = &remaining[..item_end];
        if let Some(title) = element_text(item, "title") {
            let title = decode_xml_entities(strip_cdata(title).trim());
            let title = collapse_whitespace(&title);
            if !title.is_empty() && !headlines.contains(&title) {
                headlines.push(title);
                if headlines.len() == limit {
                    break;
                }
            }
        }
        remaining = &remaining[item_end + "</item>".len()..];
    }
    headlines
}

fn element_text<'a>(xml: &'a str, element: &str) -> Option<&'a str> {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn strip_cdata(value: &str) -> &str {
    value
        .strip_prefix("<![CDATA[")
        .and_then(|value| value.strip_suffix("]]>"))
        .unwrap_or(value)
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_parser_skips_channel_title_decodes_and_deduplicates_items() {
        let xml = r#"<?xml version="1.0"?><rss><channel><title>Feed title</title>
          <item><title><![CDATA[First &amp; foremost]]></title></item>
          <item><title>Second   headline</title></item>
          <item><title><![CDATA[First &amp; foremost]]></title></item>
        </channel></rss>"#;
        assert_eq!(
            parse_rss_headlines(xml, 5),
            vec!["First & foremost", "Second headline"]
        );
        assert_eq!(parse_rss_headlines(xml, 1), vec!["First & foremost"]);
    }

    #[test]
    fn malformed_feed_returns_no_headlines() {
        assert!(parse_rss_headlines("<rss><title>Only channel</title>", 5).is_empty());
    }
}
