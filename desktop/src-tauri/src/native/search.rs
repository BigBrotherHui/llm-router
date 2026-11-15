//! 网关侧联网能力（server_tool_use 的执行后端）。
//!
//! OpenAI 兼容的上游本身没有 web_search / web_fetch 工具（实测：把 web_search 当函数工具发过去
//! 直接被上游 400 拒掉），所以当客户端（DeepSeek 官方前端 / chat-demo）声明
//! `deepseek_web_search` / `deepseek_read_url` 这类「服务端工具」时，执行方只能是网关这一层。
//! 网关执行完再产回 Anthropic 的服务端工具块：
//!   deepseek_web_search -> server_tool_use(kind=search)    + web_search_tool_result
//!   deepseek_read_url   -> server_tool_use(kind=read_link) + web_fetch_tool_result
//!
//! 搜索后端选 Bing 的 HTML 结果页（本机实测可达：bing.com 200 / 10 条 b_algo；
//! html.duckduckgo.com、lite.duckduckgo.com 均超时，search.marcia.cc 连接被重置）。
//! 解析全部手写，不引 regex / scraper —— 与项目其它模块的风格保持一致。

use serde_json::{json, Value};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGE_CHARS: usize = 20_000;
const MAX_SNIPPET: usize = 500;

/// 一条搜索结果。
#[derive(Debug, Clone, Default)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub site: Option<String>,
    pub icon: Option<String>,
}

/// CJK 友好的截断（避免在多字节字符中间切开）。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != '&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest: String = bytes[i..].iter().take(12).collect();
        let semi = rest.find(';');
        let mut handled = false;
        if let Some(pos) = semi {
            let ent: String = rest[1..pos].to_string();
            let replacement = match ent.as_str() {
                "amp" => Some("&".to_string()),
                "lt" => Some("<".to_string()),
                "gt" => Some(">".to_string()),
                "quot" => Some("\"".to_string()),
                "apos" => Some("'".to_string()),
                "nbsp" => Some(" ".to_string()),
                "ensp" | "emsp" | "thinsp" => Some(" ".to_string()),
                "middot" => Some("·".to_string()),
                "hellip" => Some("…".to_string()),
                _ => {
                    if let Some(num) = ent.strip_prefix('#') {
                        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            num.parse::<u32>().ok()
                        };
                        code.and_then(char::from_u32).map(|c| c.to_string())
                    } else {
                        None
                    }
                }
            };
            if let Some(r) = replacement {
                out.push_str(&r);
                i += pos + 1;
                handled = true;
            }
        }
        if !handled {
            out.push('&');
            i += 1;
        }
    }
    out
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "br"
            | "div"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "table"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "blockquote"
            | "dd"
            | "dt"
            | "pre"
            | "hr"
    )
}

/// HTML -> 纯文本：跳过 script/style/noscript，块级标签换行，其余标签丢弃，最后折叠空白。
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_lowercase();
    let mut i = 0usize;
    while i < html.len() {
        let c = html.as_bytes()[i];
        if c == b'<' {
            let mut skipped = false;
            for skip in ["<script", "<style", "<noscript", "<svg", "<head"] {
                if lower[i..].starts_with(skip) {
                    let close = format!("</{}", &skip[1..]);
                    if let Some(p) = lower[i..].find(&close) {
                        let after = i + p;
                        if let Some(gt) = html[after..].find('>') {
                            i = after + gt + 1;
                            skipped = true;
                            break;
                        }
                    }
                }
            }
            if skipped {
                continue;
            }
            let Some(gt) = html[i..].find('>') else {
                break;
            };
            let tag_raw = &html[i + 1..i + gt];
            let name: String = tag_raw
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if is_block_tag(&name) {
                out.push('\n');
            }
            i += gt + 1;
            continue;
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    collapse(&decode_entities(&out))
}

/// 折叠空白：连续空格合一，最多保留一个空行。
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    let mut last_newline = false;
    for c in s.chars() {
        match c {
            '\n' | '\r' => {
                if !last_newline && !out.is_empty() {
                    out.push('\n');
                    last_newline = true;
                }
                last_space = false;
            }
            c if c.is_whitespace() => {
                if !last_space && !last_newline {
                    out.push(' ');
                }
                last_space = true;
            }
            c => {
                out.push(c);
                last_space = false;
                last_newline = false;
            }
        }
    }
    out.trim().to_string()
}

/// 干净的展示文本（标题/摘要用）：去标签 + 解实体 + 折叠空白。
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        if s.as_bytes()[i] == b'<' {
            match s[i..].find('>') {
                Some(gt) => {
                    i += gt + 1;
                    continue;
                }
                None => break,
            }
        }
        let c = s[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    collapse(&decode_entities(&out))
}

/// 摘要里 Bing 会带一个日期前缀，形如「2025年11月30日 · 正文」或「2 天之前‎ 正文」。
fn clean_snippet(s: &str) -> String {
    let t = clean_text(s);
    for sep in ['\u{200e}', '·'] {
        if let Some(pos) = t.find(sep) {
            let head: String = t[..pos].chars().collect();
            let tail = t[pos + sep.len_utf8()..].trim();
            let looks_like_date = head.contains('年')
                || head.contains("之前")
                || head.contains("ago")
                || (!head.is_empty()
                    && head
                        .chars()
                        .all(|c| c.is_ascii_digit() || " 年月日时分秒天小".contains(c)));
            if looks_like_date && !tail.is_empty() {
                return truncate_chars(tail, MAX_SNIPPET);
            }
        }
    }
    truncate_chars(&t, MAX_SNIPPET)
}

fn between(s: &str, start: &str, end: &str) -> Option<String> {
    let p = s.find(start)? + start.len();
    let rest = &s[p..];
    let q = rest.find(end)?;
    Some(rest[..q].to_string())
}

/// 从 `<p ...>` 开标签后取到 `</p>`。
fn text_of_p(s: &str) -> Option<String> {
    let p = s.find("<p")?;
    let rest = &s[p..];
    let gt = rest.find('>')?;
    let after = &rest[gt + 1..];
    let end = after.find("</p>")?;
    Some(clean_text(&after[..end]))
}

/// 解析一个 `<li class="b_algo">` 结果块。
fn parse_result(chunk: &str) -> Option<Hit> {
    let h2 = chunk.find("<h2")?;
    let head = &chunk[h2..];
    let href_at = head.find("href=\"")? + 6;
    let url_raw = &head[href_at..];
    let url = decode_entities(&url_raw[..url_raw.find('"')?]);
    if !url.starts_with("http") {
        return None;
    }
    let gt = head[href_at..].find('>')? + href_at;
    let title_html = &head[gt + 1..];
    let title = clean_text(&title_html[..title_html.find("</a>")?]);
    let snippet = chunk
        .find("b_lineclamp")
        .and_then(|i| text_of_p(&chunk[i..]))
        .or_else(|| {
            chunk
                .find("class=\"b_caption\"")
                .and_then(|i| text_of_p(&chunk[i..]))
        })
        .map(|s| clean_snippet(&s))
        .unwrap_or_default();
    let site = between(chunk, "class=\"tptt\">", "</div>")
        .map(|s| clean_text(&s))
        .filter(|s| !s.is_empty());
    let icon = between(chunk, "data-src=\"", "\"")
        .map(|s| decode_entities(&s))
        .filter(|s| s.starts_with("http"));
    if title.is_empty() && snippet.is_empty() {
        return None;
    }
    Some(Hit {
        title,
        url,
        snippet,
        site,
        icon,
    })
}

fn parse_bing(html: &str, limit: usize) -> Vec<Hit> {
    let needle = "<li class=\"b_algo";
    let mut hits: Vec<Hit> = Vec::new();
    let mut pos = 0usize;
    while hits.len() < limit {
        let Some(found) = html[pos..].find(needle) else {
            break;
        };
        let start = pos + found + needle.len();
        let end = html[start..]
            .find(needle)
            .map(|e| start + e)
            .or_else(|| html[start..].find("</ol>").map(|e| start + e))
            .unwrap_or(html.len())
            .min(html.len());
        if end <= start {
            break;
        }
        if let Some(h) = parse_result(&html[start..end]) {
            if !hits.iter().any(|x| x.url == h.url) {
                hits.push(h);
            }
        }
        pos = end;
    }
    hits
}

/// 联网搜索（Bing HTML 结果页）。
pub async fn web_search(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    let q = truncate_chars(query.trim(), 300);
    if q.is_empty() {
        return Err("搜索关键词为空".to_string());
    }
    let url = format!(
        "https://www.bing.com/search?q={}&setlang=zh-CN&mkt=zh-CN&count=20",
        percent_encode(&q)
    );
    let resp = client
        .get(&url)
        .header("user-agent", UA)
        .header("accept", "text/html,application/xhtml+xml")
        .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("搜索请求失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("搜索后端返回 HTTP {}", status.as_u16()));
    }
    let html = resp.text().await.map_err(|e| format!("读取搜索结果失败: {e}"))?;
    let hits = parse_bing(&html, limit);
    if hits.is_empty() {
        if html.contains("b_no") || html.contains("没有与此相关的结果") || html.len() < 20_000 {
            return Ok(Vec::new());
        }
        return Err("搜索结果解析为空（可能被反爬拦截）".to_string());
    }
    Ok(hits)
}

/// 抓取网页正文（server_tool_use kind=read_link 的执行体）。
pub async fn read_page(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Option<String>, String), String> {
    let resp = client
        .get(url)
        .header("user-agent", UA)
        .header("accept", "text/html,application/xhtml+xml")
        .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("抓取失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("网页返回 HTTP {}", status.as_u16()));
    }
    let raw = resp
        .bytes()
        .await
        .map_err(|e| format!("读取网页失败: {e}"))?;
    let raw = if raw.len() > MAX_PAGE_BYTES {
        &raw[..MAX_PAGE_BYTES]
    } else {
        &raw[..]
    };
    let html = String::from_utf8_lossy(raw).to_string();
    let title = between(&html, "<title", "</title>")
        .map(|s| clean_text(&s.rsplit('>').next().unwrap_or(&s)))
        .filter(|s| !s.is_empty());
    let text = truncate_chars(&strip_html(&html), MAX_PAGE_CHARS);
    if text.is_empty() {
        return Err("网页正文为空（可能是纯前端渲染的页面）".to_string());
    }
    Ok((title, text))
}

/// 正文里出现的第一个 http(s) 链接（deepseek_read_url 的触发条件）。
pub fn first_url(text: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        let mut from = 0usize;
        while let Some(pos) = text[from..].find(scheme) {
            let start = from + pos;
            let tail = &text[start..];
            let end = tail
                .char_indices()
                .find(|(_, c)| c.is_whitespace() || matches!(c, '）' | ')' | '】' | ']' | '>' | '"' | '\'' | '，' | ',' | '。'))
                .map(|(i, _)| i)
                .unwrap_or(tail.len());
            let url = tail[..end].trim_end_matches(['.', ';', ':', '!', '?']).to_string();
            if url.len() > scheme.len() + 3 {
                return Some(url);
            }
            from = start + scheme.len();
        }
    }
    None
}

/// 搜索结果 -> web_search_result 块里的条目。
pub fn hit_to_result(hit: &Hit) -> Value {
    json!({
        "type": "web_search_result",
        "title": hit.title,
        "url": hit.url,
        "snippet": if hit.snippet.is_empty() { Value::Null } else { json!(hit.snippet) },
        "site_name": hit.site.clone().map(Value::from).unwrap_or(Value::Null),
        "site_icon": hit.icon.clone().map(Value::from).unwrap_or(Value::Null),
        "encrypted_content": "",
        "page_age": Value::Null,
    })
}

/// 给上游模型看的搜索结果上下文（编号 1..n，与 citations 的编号对齐）。
pub fn render_context(query: &str, hits: &[Hit]) -> String {
    let mut out = String::new();
    out.push_str("【联网搜索结果（由网关实时抓取，编号即引用编号）】\n");
    out.push_str(&format!("搜索词：{query}\n"));
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {}\n{}\n{}\n",
            i + 1,
            if h.title.is_empty() { "(无标题)" } else { &h.title },
            h.url,
            truncate_chars(&h.snippet, 300)
        ));
    }
    out.push_str(
        "引用要求：正文里引用上面某条来源时，紧跟该句末尾写角标 [编号]（可连写 [1][2]），\
不要写 Markdown 链接，也不要写「来源 1」这类文字说明。回答中至少给出一次这样的角标。\n",
    );
    out
}

#[cfg(test)]
mod tests {
    /// 手工夹具：解析一段 Bing 结果页片段（不联网）。
    #[test]
    fn parse_fixture() {
        let html = r#"<ol id="b_results"><li class="b_algo" data-id iid=SERP.5334><div class="b_tpcn"><a class="tilk" href="https://www.deepseek.com/news/"><div class="tptxt"><div class="tptt">deepseek.com</div></div></a></div><h2><a target="_blank" href="https://www.deepseek.com/news/" h="ID=SERP,5128.2"><strong>DeepSeek</strong> | Research &amp; News</a></h2><div class="b_caption"><p class="b_lineclamp2">2025年11月30日&#0183;&#32;DeepSeek Research &amp; News — Latest research</p></div><img data-src="https://ts1.tc.mm.bing.net/th/id/ODLS.X?w=32"></li></ol>"#;
        let hits = super::parse_bing(html, 5);
        assert_eq!(hits.len(), 1, "应解析出一条结果");
        assert_eq!(hits[0].url, "https://www.deepseek.com/news/");
        assert_eq!(hits[0].title, "DeepSeek | Research & News");
        assert_eq!(hits[0].site.as_deref(), Some("deepseek.com"));
        assert!(hits[0].snippet.starts_with("DeepSeek Research"), "摘要应去掉日期前缀: {}", hits[0].snippet);
    }

    #[test]
    fn url_detect() {
        assert_eq!(
            super::first_url("看看 https://example.com/a?b=1，然后总结"),
            Some("https://example.com/a?b=1".to_string())
        );
        assert_eq!(super::first_url("没有链接"), None);
    }
}
