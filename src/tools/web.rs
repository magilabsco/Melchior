//! `web_fetch` (plugin `tool-web`): fetch a URL and return readable text.

use std::time::Duration;

use super::{Tool, ToolCx, ToolOutput, ToolSpec, opt_i64, prop, schema, str_arg, try_out};
use crate::http::{self, Request};
use crate::jobj;
use crate::json::Json;
use crate::util;

pub struct WebFetch;

impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch an http(s) URL and return its text content (HTML is reduced to readable text).".into(),
            parameters: schema(
                jobj! {
                    "url" => prop("string", "Absolute http:// or https:// URL."),
                    "max_chars" => prop("integer", "Maximum characters to return (default 40000)."),
                },
                &["url"],
            ),
        }
    }

    fn call(&self, _cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let mut url = try_out!(str_arg(args, "url")).to_string();
        let max = opt_i64(args, "max_chars").unwrap_or(40_000).clamp(500, 200_000) as usize;
        for _ in 0..5 {
            let request = Request::new("GET", &url)
                .header("Accept", "text/html,text/plain,application/json;q=0.9,*/*;q=0.5")
                .timeouts(Duration::from_secs(30), Duration::from_secs(60));
            let response = match http::send(&request) {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("fetch failed: {e}")),
            };
            if (300..400).contains(&response.status)
                && let Some(location) = response.header("location") {
                    url = absolute(&url, location);
                    continue;
                }
            let status = response.status;
            let content_type = response.header("content-type").unwrap_or("").to_ascii_lowercase();
            let body = match response.text(8 * 1024 * 1024) {
                Ok(body) => body,
                Err(e) => return ToolOutput::err(format!("fetch failed: {e}")),
            };
            let text = if content_type.contains("html") || body.trim_start().starts_with('<') { html_to_text(&body) } else { body };
            let text = util::truncate(text.trim(), max, "\n…[truncated]");
            return if status >= 400 {
                ToolOutput::err(format!("HTTP {status} from {url}\n{text}"))
            } else {
                ToolOutput::ok(format!("{url} (HTTP {status})\n\n{text}"))
            };
        }
        ToolOutput::err("too many redirects")
    }
}

fn absolute(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(base.len());
    if location.starts_with('/') {
        format!("{}{location}", &base[..host_end])
    } else {
        let dir_end = base.rfind('/').filter(|i| *i >= host_end).unwrap_or(host_end);
        format!("{}/{location}", &base[..dir_end])
    }
}

/// Reduce HTML to text: drop script/style/head, turn block tags into line
/// breaks, decode common entities, and collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &lower[i..];
            let mut skipped = false;
            for tag in ["script", "style", "noscript", "svg", "head"] {
                if rest.starts_with(&format!("<{tag}")) {
                    if let Some(end) = rest.find(&format!("</{tag}>")) {
                        i += end + tag.len() + 3;
                        skipped = true;
                    }
                    break;
                }
            }
            if skipped {
                continue;
            }
            let end = rest.find('>').map(|e| i + e + 1).unwrap_or(bytes.len());
            let tag = &lower[i..end];
            if ["<br", "<p", "</p", "<div", "</div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "</h", "<table", "</table"]
                .iter()
                .any(|t| tag.starts_with(t))
            {
                out.push('\n');
            } else if tag.starts_with("<td") || tag.starts_with("<th") {
                out.push('\t');
            }
            i = end;
            continue;
        }
        let next = html[i..].find('<').map(|n| i + n).unwrap_or(html.len());
        out.push_str(&html[i..next]);
        i = next;
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut lines = Vec::new();
    let mut blank = false;
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !blank && !lines.is_empty() {
                lines.push(String::new());
            }
            blank = true;
        } else {
            lines.push(line);
            blank = false;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let text = html_to_text("<html><head><title>x</title></head><body><script>bad()</script><h1>Title</h1><p>Hello &amp; welcome</p></body></html>");
        assert_eq!(text, "Title\n\nHello & welcome");
    }

    #[test]
    fn resolves_redirects() {
        assert_eq!(absolute("https://a.com/x/y", "/z"), "https://a.com/z");
        assert_eq!(absolute("https://a.com/x/y", "z"), "https://a.com/x/z");
    }
}
