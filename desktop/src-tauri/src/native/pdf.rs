//! PDF 文本抽取（尽力而为，无外部解析库）。
//!
//! 为什么放在网关：DeepSeek 官方前端会把 PDF 传到 Files API 再以 `document` 块引用，
//! 而本网关的 OpenAI 兼容上游明确拒绝 PDF 输入（实测 `The current model does not support
//! PDF file input.`）。所以 PDF 必须在网关侧先抽成文本，再作为普通文本喂给上游。
//!
//! 覆盖范围与边界（如实说明）：
//!   - 能处理：未压缩 / FlateDecode 压缩的内容流；标准字体简单编码；带 ToUnicode CMap 的
//!     子集字体（Identity-H 等 CID 字体，中文 PDF 大多是这种）；`(...)Tj`、`[...]TJ`、
//!     `'`、`"` 这些显示操作符；十六进制字符串与 UTF-16BE 字符串。
//!   - 处理不了：扫描件（纯图片 PDF，没有文本层）、没有 ToUnicode 表的自定义编码字体、
//!     加密 PDF。这些情况返回 None，由调用方明确告知客户端，而不是静默丢成一个空附件。

use std::collections::HashMap;
use std::io::Read;

const MAX_CHARS: usize = 120_000;

/// 抽取 PDF 文本。抽不出来返回 None（调用方据此给出明确错误）。
pub fn extract_text(bytes: &[u8]) -> Option<String> {
    if !bytes.starts_with(b"%PDF") {
        return None;
    }
    let cmap = tounicode_map(bytes);
    let mut out = String::new();
    for content in content_streams(bytes) {
        let text = text_from_content(&content, &cmap);
        if !text.trim().is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text.trim());
        }
        if out.chars().count() >= MAX_CHARS {
            break;
        }
    }
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(MAX_CHARS).collect())
    }
}

/// 收集文件里所有 ToUnicode CMap（子集字体的十六进制串存的是 CID，
/// 不查这张表只会得到乱码——实测一份中文 PDF 就是这种情况）。
fn tounicode_map(bytes: &[u8]) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    for (_, body) in object_bodies(bytes) {
        if find_sub(body, b"stream").is_none() {
            continue;
        }
        let head = &body[..body.len().min(512)];
        let head_s = String::from_utf8_lossy(head);
        if !(head_s.contains("/FlateDecode") || head_s.contains("/Fl") || head_s.contains("/ToUnicode")) {
            continue;
        }
        if let Some(data) = stream_data(body) {
            if find_sub(&data, b"begincmap").is_some() {
                parse_cmap(&data, &mut map);
            }
        }
    }
    map
}

/// 扫描 `<数字> 0 obj ... endobj`，返回（对象号, 体）。
fn object_bodies(bytes: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_digit()) {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let num = std::str::from_utf8(&bytes[i..j]).ok().and_then(|s| s.parse::<u32>().ok());
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if bytes.get(k) == Some(&b'0') {
                let mut m = k + 1;
                while m < bytes.len() && bytes[m].is_ascii_whitespace() {
                    m += 1;
                }
                if bytes[m..].starts_with(b"obj") {
                    let start = m + 3;
                    let end = find_sub(&bytes[start..], b"endobj")
                        .map(|e| start + e)
                        .unwrap_or(bytes.len());
                    if let Some(n) = num {
                        out.push((n, &bytes[start..end]));
                    }
                    i = end.saturating_add(1);
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// 取对象里的流（按字典决定是否 FlateDecode 解压）。
fn stream_data(body: &[u8]) -> Option<Vec<u8>> {
    let at = find_sub(body, b"stream")?;
    let mut start = at + "stream".len();
    if body[start..].starts_with(b"\r\n") {
        start += 2;
    } else if body[start..].starts_with(b"\n") {
        start += 1;
    }
    let end = start + find_sub(&body[start..], b"endstream")?;
    let dict = String::from_utf8_lossy(&body[..at]);
    let raw = &body[start..end];
    if dict.contains("/FlateDecode") || dict.contains("/Fl ") {
        inflate(raw)
    } else {
        Some(raw.to_vec())
    }
}

#[derive(Debug, Clone)]
enum Tok {
    Hex(Vec<u8>),
    Word(String),
    ArrStart,
    ArrEnd,
}

fn tokenize_cmap(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'<' => {
                if b.get(i + 1) == Some(&b'<') {
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                let mut nibbles = Vec::new();
                while j < b.len() && b[j] != b'>' {
                    let c = b[j];
                    let v = match c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        b'A'..=b'F' => c - b'A' + 10,
                        _ => {
                            j += 1;
                            continue;
                        }
                    };
                    nibbles.push(v);
                    j += 1;
                }
                let mut bytes = Vec::new();
                let mut k = 0usize;
                while k + 1 < nibbles.len() {
                    bytes.push((nibbles[k] << 4) | nibbles[k + 1]);
                    k += 2;
                }
                if k < nibbles.len() {
                    bytes.push(nibbles[k] << 4);
                }
                out.push(Tok::Hex(bytes));
                i = j + 1;
            }
            b'>' => i += 1,
            b'[' => {
                out.push(Tok::ArrStart);
                i += 1;
            }
            b']' => {
                out.push(Tok::ArrEnd);
                i += 1;
            }
            b'%' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let mut j = i;
                while j < b.len()
                    && !b[j].is_ascii_whitespace()
                    && !matches!(b[j], b'<' | b'>' | b'[' | b']')
                {
                    j += 1;
                }
                out.push(Tok::Word(String::from_utf8_lossy(&b[i..j]).to_string()));
                i = j;
            }
        }
    }
    out
}

fn code_of(b: &[u8]) -> Option<u16> {
    match b.len() {
        0 => None,
        1 => Some(b[0] as u16),
        _ => Some(u16::from_be_bytes([b[0], b[1]])),
    }
}

fn utf16_units(b: &[u8]) -> Vec<u16> {
    b.chunks(2)
        .map(|c| {
            if c.len() == 2 {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                c[0] as u16
            }
        })
        .collect()
}

fn utf16_from(b: &[u8]) -> String {
    String::from_utf16_lossy(&utf16_units(b))
}

/// 解析 CMap 的 beginbfchar / beginbfrange 段，填充 CID -> 文本。
fn parse_cmap(data: &[u8], map: &mut HashMap<u16, String>) {
    let toks = tokenize_cmap(&String::from_utf8_lossy(data));
    let mut i = 0usize;
    while i < toks.len() {
        let word = match &toks[i] {
            Tok::Word(w) => w.clone(),
            _ => {
                i += 1;
                continue;
            }
        };
        if word == "beginbfchar" {
            i += 1;
            while i < toks.len() {
                if matches!(&toks[i], Tok::Word(w) if w == "endbfchar") {
                    break;
                }
                match (toks.get(i), toks.get(i + 1)) {
                    (Some(Tok::Hex(src)), Some(Tok::Hex(dst))) => {
                        if let Some(code) = code_of(src) {
                            map.insert(code, utf16_from(dst));
                        }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
        } else if word == "beginbfrange" {
            i += 1;
            while i < toks.len() {
                if matches!(&toks[i], Tok::Word(w) if w == "endbfrange") {
                    break;
                }
                let (lo, hi, third) = (toks.get(i), toks.get(i + 1), toks.get(i + 2));
                if let (Some(Tok::Hex(lo_b)), Some(Tok::Hex(hi_b)), Some(third)) = (lo, hi, third) {
                    if let (Some(a), Some(b)) = (code_of(lo_b), code_of(hi_b)) {
                        match third {
                            Tok::Hex(dst_b) => {
                                let base = utf16_units(dst_b);
                                let mut c = a as u32;
                                let mut k = 0u32;
                                while c <= b as u32 {
                                    let mut u = base.clone();
                                    if let Some(last) = u.last_mut() {
                                        *last = last.wrapping_add(k as u16);
                                    }
                                    map.insert(c as u16, String::from_utf16_lossy(&u));
                                    c += 1;
                                    k += 1;
                                }
                                i += 3;
                                continue;
                            }
                            Tok::ArrStart => {
                                let mut c = a as u32;
                                let mut j = i + 3;
                                while j < toks.len() {
                                    match &toks[j] {
                                        Tok::Hex(d) => {
                                            if c <= b as u32 {
                                                map.insert(c as u16, utf16_from(d));
                                            }
                                            c += 1;
                                            j += 1;
                                        }
                                        Tok::ArrEnd => {
                                            j += 1;
                                            break;
                                        }
                                        _ => j += 1,
                                    }
                                }
                                i = j;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
                i += 1;
            }
        }
        i += 1;
    }
}


/// 取出所有（已解压的）内容流字节。
fn content_streams(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = find_sub(&bytes[pos..], b"stream") {
        let at = pos + rel;
        let mut start = at + "stream".len();
        // 跳过 stream 关键字后的换行
        if bytes[start..].starts_with(b"\r\n") {
            start += 2;
        } else if bytes[start..].starts_with(b"\n") {
            start += 1;
        }
        let end = match find_sub(&bytes[start..], b"endstream") {
            Some(j) => start + j,
            None => break,
        };
        let raw = &bytes[start..end];
        // 往前找字典判断压缩方式
        let dict_start = at.saturating_sub(512);
        let dict = String::from_utf8_lossy(&bytes[dict_start..at]);
        let flate = dict.contains("/FlateDecode") || dict.contains("/Fl");
        let image = dict.contains("/Image");
        pos = end + "endstream".len();
        if image {
            continue;
        }
        let data = if flate {
            match inflate(raw) {
                Some(d) => d,
                None => continue,
            }
        } else {
            raw.to_vec()
        };
        // 只有像内容流的才要（含文本操作符）
        if find_sub(&data, b"BT").is_some() || find_sub(&data, b"Tj").is_some() || find_sub(&data, b"TJ").is_some() {
            out.push(data);
        }
    }
    out
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    // 有些流前后带空白，逐点尝试起点，绕过长度字节的前导噪声。
    for skip in 0..data.len().min(4) {
        let mut dec = flate2::read::ZlibDecoder::new(&data[skip..]);
        let mut buf = Vec::new();
        if dec.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            return Some(buf);
        }
    }
    // 少数 PDF 用裸 deflate（无 zlib 头）
    let mut dec = flate2::read::DeflateDecoder::new(data);
    let mut buf = Vec::new();
    if dec.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
        return Some(buf);
    }
    None
}

/// 从内容流里挑出显示用的字符串，按文本定位操作符插入换行。
///
/// 换行判断要看坐标而不是「见到 Td 就换行」：不少中文 PDF 是逐字定位的
/// （`<XXXX>Tj 403 -0 TD` 每个字一个 cell），照字面换行会得到一字一行的竖排文本。
fn text_from_content(content: &[u8], cmap: &HashMap<u16, String>) -> String {
    let mut out = String::new();
    let mut nums: Vec<f64> = Vec::new();
    let mut last_y: Option<f64> = None;
    let mut pending_space = false;
    let mut i = 0usize;
    let n = content.len();

    fn newline(out: &mut String, pending_space: &mut bool) {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        *pending_space = false;
    }

    while i < n {
        let c = content[i];
        if c == b'(' {
            let (bytes, next) = read_literal_string(content, i + 1);
            out.push_str(&decode_string_bytes(&bytes, cmap));
            i = next;
            continue;
        }
        if c == b'<' {
            if content.get(i + 1) == Some(&b'<') {
                // 字典（如 << /Filter >>），不是字符串
                nums.clear();
                i += 2;
                continue;
            }
            let close = match find_sub(&content[i + 1..], b">") {
                Some(j) => i + 1 + j,
                None => break,
            };
            out.push_str(&decode_hex(&content[i + 1..close], cmap));
            i = close + 1;
            continue;
        }
        if c == b'>' || c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.') {
            let mut j = i;
            while j < n
                && (content[j].is_ascii_digit() || matches!(content[j], b'-' | b'+' | b'.'))
            {
                j += 1;
            }
            if let Ok(v) = std::str::from_utf8(&content[i..j]).unwrap_or("").parse::<f64>() {
                nums.push(v);
                if nums.len() > 8 {
                    nums.remove(0);
                }
            }
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || matches!(c, b'\'' | b'"' | b'*') {
            let mut j = i;
            while j < n
                && (content[j].is_ascii_alphabetic()
                    || matches!(content[j], b'*' | b'\'' | b'"'))
            {
                j += 1;
            }
            let op = String::from_utf8_lossy(&content[i..j]).to_string();
            match op.as_str() {
                "Tm" => {
                    if nums.len() >= 6 {
                        let y = nums[nums.len() - 2];
                        if let Some(prev) = last_y {
                            if (y - prev).abs() > 0.5 {
                                newline(&mut out, &mut pending_space);
                            }
                        }
                        last_y = Some(y);
                    }
                }
                "Td" | "TD" => {
                    if nums.len() >= 2 {
                        let ty = nums[nums.len() - 1];
                        let tx = nums[nums.len() - 2];
                        // 纵向位移或回退到行首都算换行；同一行内的字距推进不算
                        if ty.abs() > 0.1 || tx < -0.5 {
                            newline(&mut out, &mut pending_space);
                        }
                    }
                }
                "T*" | "ET" => newline(&mut out, &mut pending_space),
                "Tj" | "TJ" => {
                    // 同一个显示调用里的多个字直接相连：逐字定位的中文 PDF 靠这条才不会
                    // 变成「基 础 研 究 院」；词间空格在原文档里是空格字形（CID 0003 -> U+0020）。
                }
                "'" | "\"" => {
                    newline(&mut out, &mut pending_space);
                }
                _ => {}
            }
            nums.clear();
            i = j;
            continue;
        }
        i += 1;
    }
    collapse(&out)
}

/// 读 PDF 字面量字符串 `( ... )`，返回（原始字节, 结束后下标）。
fn read_literal_string(content: &[u8], mut i: usize) -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    let n = content.len();
    let mut depth = 1i32;
    while i < n {
        let c = content[i];
        match c {
            b'\\' => {
                i += 1;
                if i >= n {
                    break;
                }
                let e = content[i];
                match e {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'b' => bytes.push(8),
                    b'f' => bytes.push(12),
                    b'(' => bytes.push(b'('),
                    b')' => bytes.push(b')'),
                    b'\\' => bytes.push(b'\\'),
                    b'\n' | b'\r' => {}
                    b'0'..=b'7' => {
                        let mut val = (e - b'0') as u32;
                        let mut k = 0;
                        while k < 2 && i + 1 < n && (b'0'..=b'7').contains(&content[i + 1]) {
                            i += 1;
                            val = val * 8 + (content[i] - b'0') as u32;
                            k += 1;
                        }
                        bytes.push((val & 0xFF) as u8);
                    }
                    other => bytes.push(other),
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                bytes.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
                bytes.push(b')');
                i += 1;
            }
            _ => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    (bytes, i)
}

/// 字符串字节的解释：有 ToUnicode 表且字节数为偶数时按 CID 查表，否则走通用兜底。
fn decode_string_bytes(bytes: &[u8], cmap: &HashMap<u16, String>) -> String {
    if !cmap.is_empty() && bytes.len() >= 2 && bytes.len() % 2 == 0 {
        let mut out = String::new();
        let mut hits = 0usize;
        for c in bytes.chunks(2) {
            let code = u16::from_be_bytes([c[0], c[1]]);
            if let Some(s) = cmap.get(&code) {
                out.push_str(s);
                hits += 1;
            }
        }
        // 至少命中一个才认为这是 CID 编码，否则按普通字节解释（ASCII 文本 PDF）
        if hits > 0 {
            return out;
        }
    }
    decode_pdf_bytes(bytes)
}

/// PDF 字符串的字节解释：优先 UTF-16BE（BOM 或大量 0x00），否则按 UTF-8/latin1 兜底。
fn decode_pdf_bytes(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        return utf16be(&bytes[2..]);
    }
    let zeros = bytes.iter().filter(|b| **b == 0).count();
    if bytes.len() >= 4 && zeros * 3 >= bytes.len() {
        return utf16be(bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|b| *b as char).collect(),
    }
}

fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| {
            if c.len() == 2 {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                c[0] as u16
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

fn decode_hex(hex: &[u8], cmap: &HashMap<u16, String>) -> String {
    let mut bytes = Vec::new();
    let mut cur = 0u8;
    let mut half = false;
    for c in hex {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        if !half {
            cur = v << 4;
            half = true;
        } else {
            bytes.push(cur | v);
            half = false;
        }
    }
    if half {
        bytes.push(cur);
    }
    decode_string_bytes(&bytes, cmap)
}

/// 合并空白：行内多空格压一个，连续空行压一个。
fn collapse(s: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for line in s.split('\n') {
        let trimmed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            if !out.ends_with("\n\n") && !out.is_empty() {
                out.push('\n');
            }
            last_space = true;
            continue;
        }
        out.push_str(&trimmed);
        out.push('\n');
        last_space = false;
    }
    let _ = last_space;
    out.trim().to_string()
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = hay.len() - needle.len();
    let mut i = 0;
    while i <= last {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工核对抽取质量：
    ///   $env:PDF_TEST_FILE='D:\x.pdf'; cargo test --release --manifest-path <...> dump_text -- --nocapture
    #[test]
    fn dump_text() {
        let path = match std::env::var("PDF_TEST_FILE") {
            Ok(p) => p,
            Err(_) => return,
        };
        let bytes = std::fs::read(&path).expect("read pdf");
        let cmap = tounicode_map(&bytes);
        println!("cmap entries = {}", cmap.len());
        let text = extract_text(&bytes).unwrap_or_default();
        let dump = std::env::temp_dir().join("pdf_dump.txt");
        let _ = std::fs::write(&dump, &text);
        println!("chars = {} -> {}", text.chars().count(), dump.display());
    }
}

