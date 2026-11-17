use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use regex::Regex;

use crate::encoding::{quoted_string_decode, url_encode_file_name};
use crate::game_data::{
    GameData, HtmlTagRanges, ModuleEntry, ModuleType, Modules, ModulesOfType, Passage,
    PassageAttributes, SystemModules, TextRange, UserData, UserDataEntry, UserDataType,
    CSS_MODULE_TYPE, CSS_TYPE, JS_MODULE_TYPE, JS_TYPE, PASSAGE_HTML_TAG,
};

pub fn read_from_file(path: &Path) -> Result<GameData> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    read_from_string(contents)
}

pub fn read_from_string(html_contents: String) -> Result<GameData> {
    let raw_text = html_contents.replace('\r', "");
    // Scope the VDom borrow so we can move raw_text into GameData afterwards.
    let (modules, styles, scripts, passages, csp_range, body_start, system_components) = {
        let dom = tl::parse(&raw_text, tl::ParserOptions::default())
            .map_err(|e| anyhow!("HTML parse error: {:?}", e))?;

        let modules = collect_modules(&dom, &raw_text)?;
        let styles = collect_user_data(&dom, &raw_text, &CSS_TYPE)?;
        let scripts = collect_user_data(&dom, &raw_text, &JS_TYPE)?;
        let passages = collect_passages(&dom, &raw_text)?;
        let csp_range = find_content_security_block(&dom);
        let body_tag = query_tags(&dom, "body")
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("<body> element not found"))?;
        let body_start = opening_tag_range(body_tag, dom.parser())?.end;
        let system_components = collect_system_components(
            &dom,
            &raw_text,
            body_start,
            csp_range,
            modules.as_ref(),
            &passages,
        )?;
        (
            modules,
            styles,
            scripts,
            passages,
            csp_range,
            body_start,
            system_components,
        )
    };

    Ok(GameData {
        document_raw_text: raw_text,
        styles,
        scripts,
        modules,
        passages,
        csp_range,
        body_start,
        system_components,
    })
}

// ---- tl position helpers ----
//
// tl's `boundaries()` returns (start_inclusive, end_inclusive) byte offsets
// of the full element in the source string. For example, in the string
// `<p><span>hello</span></p>`, the span element has boundaries (3, 20),
// where 3 is the `<` of `<span>` and 20 is the `>` of `</span>`.
//
// All TextRange values produced here use exclusive end (Rust convention),
// so we convert with `end + 1`.

// Pointer offset of the source buffer, derived from a tag whose `_raw` we know
// both by pointer (`tag.raw().as_ptr()`) and by byte offset (`tag.boundaries().0`).
// tl does not expose the source pointer directly.
fn source_base(tag: &tl::HTMLTag, parser: &tl::Parser) -> usize {
    let (tag_start, _) = tag.boundaries(parser);
    tag.raw().as_ptr() as usize - tag_start
}

fn node_start(node: &tl::Node, parser: &tl::Parser, base: usize) -> Result<usize> {
    match node {
        tl::Node::Tag(t) => Ok(t.boundaries(parser).0),
        tl::Node::Raw(b) | tl::Node::Comment(b) => b
            .as_bytes_borrowed()
            .map(|s| s.as_ptr() as usize - base)
            .ok_or_else(|| anyhow!("child bytes are not borrowed from source")),
    }
}

fn node_end(node: &tl::Node, parser: &tl::Parser, base: usize) -> Result<usize> {
    match node {
        tl::Node::Tag(t) => Ok(t.boundaries(parser).1 + 1),
        tl::Node::Raw(b) | tl::Node::Comment(b) => b
            .as_bytes_borrowed()
            .map(|s| s.as_ptr() as usize - base + s.len())
            .ok_or_else(|| anyhow!("child bytes are not borrowed from source")),
    }
}

fn inner_content_range(tag: &tl::HTMLTag, parser: &tl::Parser) -> Result<TextRange> {
    // Derive the inner range from the byte positions of the first and last
    // child nodes, rather than assuming a fixed `</tagname>` close-tag length.
    // This handles whitespace inside the closing tag, uppercase close tags,
    // multiline opening tags, etc.
    let children = tag.children();
    let top = children.top().as_slice();

    if let (Some(first), Some(last)) = (top.first(), top.last()) {
        let base = source_base(tag, parser);
        let first_node = first
            .get(parser)
            .ok_or_else(|| anyhow!("invalid child node handle"))?;
        let last_node = last
            .get(parser)
            .ok_or_else(|| anyhow!("invalid child node handle"))?;
        let start = node_start(first_node, parser, base)?;
        let end = node_end(last_node, parser, base)?;
        return Ok(TextRange::new(start, end));
    }

    // Empty element: inner range is empty, positioned just past the opening `>`.
    let end = opening_tag_end(tag, parser)?;
    Ok(TextRange::new(end, end))
}

fn outer_range(tag: &tl::HTMLTag, parser: &tl::Parser) -> Result<TextRange> {
    let (start, end) = tag.boundaries(parser);
    Ok(TextRange::new(start, end + 1))
}

// Byte offset just past the `>` of the opening tag.
//
// Can't use `tag.raw().len()`: contrary to what its doc comment says, `raw()`
// returns the full outer HTML (opening tag + content + closing tag) whenever
// tl successfully matched a closing tag. We scan the tag's raw bytes for the
// opening `>` instead, which works whether `raw()` is the full element or only
// the opening tag (as happens when tl couldn't find a matching close).
//
// Per the HTML5 spec, `>` may appear unescaped inside quoted attribute values
// (e.g. `<p title="x>y">`), so we skip over `"..."` and `'...'` regions.
fn opening_tag_end(tag: &tl::HTMLTag, parser: &tl::Parser) -> Result<usize> {
    let (outer_start, _) = tag.boundaries(parser);
    let mut quote: Option<u8> = None;
    for (i, &b) in tag.raw().as_bytes().iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Ok(outer_start + i + 1),
                _ => {}
            },
        }
    }
    Err(anyhow!("malformed opening tag: no unquoted '>' found"))
}

// Inner/outer ranges for a "raw text" element (`<script>`, `<style>`) whose
// contents must not be interpreted as HTML. tl is not an HTML5-spec parser and
// happily tries to parse `<` inside script/style bodies as tags, then gets
// confused and truncates the element's children — so we can't rely on
// `inner_content_range` here. Instead, scan the source directly for
// `</tagname>` (ASCII case-insensitive, optional whitespace before `>`).
fn raw_text_element_ranges(
    tag: &tl::HTMLTag,
    parser: &tl::Parser,
    source: &str,
) -> Result<HtmlTagRanges> {
    let (outer_start, _) = tag.boundaries(parser);
    let inner_start = opening_tag_end(tag, parser)?;
    let tag_name = tag.name().as_utf8_str();
    let close = find_raw_text_close_tag(source, inner_start, &tag_name)?;
    Ok(HtmlTagRanges {
        inner: TextRange::new(inner_start, close.start),
        outer: TextRange::new(outer_start, close.end),
    })
}

fn find_raw_text_close_tag(source: &str, from: usize, tag_name: &str) -> Result<TextRange> {
    let name_bytes = tag_name.as_bytes();
    let tail = &source.as_bytes()[from..];
    let mut i = 0;
    while i + 2 + name_bytes.len() < tail.len() {
        if tail[i] == b'<' && tail[i + 1] == b'/' {
            let after_slash = i + 2;
            let name_end = after_slash + name_bytes.len();
            if tail[after_slash..name_end]
                .iter()
                .zip(name_bytes)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                let mut j = name_end;
                while j < tail.len() && matches!(tail[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                if j < tail.len() && tail[j] == b'>' {
                    return Ok(TextRange::new(from + i, from + j + 1));
                }
            }
        }
        i += 1;
    }
    Err(anyhow!(
        "no </{tag_name}> close tag found after position {from}"
    ))
}

fn opening_tag_range(tag: &tl::HTMLTag, parser: &tl::Parser) -> Result<TextRange> {
    let (start, _) = tag.boundaries(parser);
    Ok(TextRange::new(start, opening_tag_end(tag, parser)?))
}

// ---- Query helpers ----

// Returns None if the attribute is absent; Some(value) if present. A valueless
// attribute like `<tag foo>` maps to Some(""), matching an empty-string value.
fn attr_string(attrs: &tl::Attributes, name: &str) -> Option<String> {
    attrs
        .get(name)
        .map(|v| v.map(|b| b.as_utf8_str().to_string()).unwrap_or_default())
}

fn find_element_by_id<'a>(dom: &'a tl::VDom, id: &str) -> Result<&'a tl::HTMLTag<'a>> {
    let handle = dom
        .get_element_by_id(id)
        .ok_or_else(|| anyhow!("Element #{} not found", id))?;
    handle
        .get(dom.parser())
        .and_then(|n| n.as_tag())
        .ok_or_else(|| anyhow!("Node #{} is not a tag element", id))
}

fn query_tags<'a>(dom: &'a tl::VDom, selector: &str) -> Vec<&'a tl::HTMLTag<'a>> {
    let Some(iter) = dom.query_selector(selector) else {
        return Vec::new();
    };
    iter.filter_map(|h| h.get(dom.parser())?.as_tag())
        .collect()
}

// ---- Collection functions ----

fn collect_modules(dom: &tl::VDom, raw_text: &str) -> Result<Option<Modules>> {
    let script_modules = collect_modules_of_type(dom, raw_text, &JS_MODULE_TYPE)?;
    let style_modules = collect_modules_of_type(dom, raw_text, &CSS_MODULE_TYPE)?;

    if script_modules.entries.is_empty() && style_modules.entries.is_empty() {
        return Ok(None);
    }

    let all_entries = script_modules
        .entries
        .iter()
        .chain(style_modules.entries.iter());
    let first_start = all_entries.clone().map(|e| e.ranges.outer.start).min();
    let last_end = all_entries.map(|e| e.ranges.outer.end).max();

    match (first_start, last_end) {
        (Some(start), Some(end)) => Ok(Some(Modules {
            html_range: TextRange::new(start, end),
            script_modules,
            style_modules,
        })),
        _ => Ok(None),
    }
}

fn collect_modules_of_type(
    dom: &tl::VDom,
    raw_text: &str,
    module_type: &'static ModuleType,
) -> Result<ModulesOfType> {
    // Note: the Kotlin original scopes this query to <head>. tl may not support
    // descendant combinators, so we query globally. For well-formed Twine HTML
    // these elements only exist in <head>.
    let selector = format!("[id^='{}-']", module_type.directory_name);
    let tags = query_tags(dom, &selector);
    let parser = dom.parser();
    let prefix_len = module_type.directory_name.len() + 1; // "dirname-"

    let mut seen_count: HashMap<String, usize> = HashMap::new();
    let mut entries = Vec::new();

    for tag in tags {
        let id = tag
            .attributes()
            .id()
            .map(|b| b.as_utf8_str().to_string())
            .ok_or_else(|| anyhow!("Module element missing id attribute"))?;

        let original_name = id[prefix_len..].to_string();

        let count = seen_count.entry(original_name.clone()).or_insert(0);
        *count += 1;

        let internal_name = if *count == 1 {
            original_name.clone()
        } else {
            format!("{}-{}", original_name, *count)
        };

        entries.push(ModuleEntry {
            original_module_name: original_name,
            module_name: internal_name,
            ranges: raw_text_element_ranges(tag, parser, raw_text)?,
        });
    }

    Ok(ModulesOfType {
        module_type,
        entries,
    })
}

fn collect_user_data(dom: &tl::VDom, raw_text: &str, data_type: &UserDataType) -> Result<UserData> {
    let tag = find_element_by_id(dom, data_type.html_id)?;
    let ranges = raw_text_element_ranges(tag, dom.parser(), raw_text)?;
    let content_range = ranges.inner;
    let outer = ranges.outer;
    let user_data_text = &raw_text[content_range.start..content_range.end];

    let pattern = format!(
        r#"(?:\n|^)/\* {} #(\d+): "(.+)" \*/\n"#,
        regex::escape(data_type.html_id)
    );
    let re = Regex::new(&pattern)?;
    let captures: Vec<_> = re.captures_iter(user_data_text).collect();

    if captures.is_empty() {
        return Ok(UserData {
            user_data_raw_text: user_data_text.to_string(),
            entries: vec![UserDataEntry {
                disk_file_name: data_type.default_file_name.to_string(),
                file_name: data_type.default_file_name.to_string(),
                index: 1,
                raw_text_range: TextRange::new(0, user_data_text.len()),
            }],
            is_split: false,
            ranges: HtmlTagRanges {
                inner: content_range,
                outer,
            },
        });
    }

    let entries = captures
        .iter()
        .enumerate()
        .map(|(i, cap)| {
            let start = cap.get(0).unwrap().end();
            let end = captures
                .get(i + 1)
                .map(|next| next.get(0).unwrap().start())
                .unwrap_or(user_data_text.len());

            let file_name = quoted_string_decode(&cap[2]);

            UserDataEntry {
                disk_file_name: url_encode_file_name(&file_name),
                file_name,
                index: cap[1].parse::<usize>().unwrap(),
                raw_text_range: TextRange::new(start, end),
            }
        })
        .collect();

    Ok(UserData {
        user_data_raw_text: user_data_text.to_string(),
        entries,
        is_split: true,
        ranges: HtmlTagRanges {
            inner: content_range,
            outer,
        },
    })
}

fn collect_passages(dom: &tl::VDom, raw_text: &str) -> Result<Vec<Passage>> {
    let tags = query_tags(dom, PASSAGE_HTML_TAG);
    let parser = dom.parser();
    let mut passages = Vec::new();

    for tag in tags {
        let attrs = tag.attributes();

        let name = attrs
            .get("name")
            .flatten()
            .map(|b| b.as_utf8_str().to_string())
            .ok_or_else(|| anyhow!("Passage missing 'name' attribute"))?;

        let attributes = PassageAttributes {
            tags: attr_string(attrs, "tags"),
            position: attr_string(attrs, "position"),
            pid: attr_string(attrs, "pid"),
        };

        let tags_list: Vec<String> = attributes
            .tags
            .as_deref()
            .unwrap_or("")
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        let is_widget = tags_list.iter().any(|t| t == "widget");
        let ranges = HtmlTagRanges {
            inner: inner_content_range(tag, parser)?,
            outer: outer_range(tag, parser)?,
        };
        let encoded_html = &raw_text[ranges.inner.start..ranges.inner.end];
        let decoded_text = text_content(encoded_html);

        passages.push(Passage {
            name,
            attributes,
            tags: tags_list,
            is_widget,
            ranges,
            decoded_text,
        });
    }

    Ok(passages)
}

/// Locate the optional SugarCube engine `<script>` blocks the extractor
/// pulls out as standalone files. Each block is only accepted when it
/// sits in its expected slot: `script_libraries` must precede `<body>`,
/// the CSP meta, and the modules section; `sugarcube-script` must come
/// after every passage. Anything outside those slots is left embedded
/// in the document and surfaced as `None` so the merger doesn't try to
/// splice it back into the wrong spot.
fn collect_system_components(
    dom: &tl::VDom,
    raw_text: &str,
    body_start: usize,
    csp_range: Option<TextRange>,
    modules: Option<&Modules>,
    passages: &[Passage],
) -> Result<SystemModules> {
    let parser = dom.parser();

    let resolve_script = |id: &str| -> Result<Option<HtmlTagRanges>> {
        let Some(handle) = dom.get_element_by_id(id) else {
            return Ok(None);
        };
        let Some(tag) = handle.get(parser).and_then(|n| n.as_tag()) else {
            return Ok(None);
        };
        if !tag.name().as_utf8_str().eq_ignore_ascii_case("script") {
            return Ok(None);
        }
        Ok(Some(raw_text_element_ranges(tag, parser, raw_text)?))
    };

    let sugarcube_libraries = resolve_script("script-libraries")?.filter(|r| {
        let upper_bound = [
            Some(body_start),
            csp_range.map(|c| c.start),
            modules.map(|m| m.html_range.start),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(usize::MAX);
        r.outer.end <= upper_bound
    });

    let last_passage_end = passages
        .iter()
        .map(|p| p.ranges.outer.end)
        .max()
        .unwrap_or(0);
    let sugarcube_script =
        resolve_script("script-sugarcube")?.filter(|r| r.outer.start >= last_passage_end);

    Ok(SystemModules {
        sugarcube_libraries,
        sugarcube_script,
    })
}

/// Find a `<meta http-equiv="Content-Security-Policy">` element and return
/// its tag range (void element, so opening tag = entire element).
fn find_content_security_block(dom: &tl::VDom) -> Option<TextRange> {
    let iter = dom.query_selector("[http-equiv='Content-Security-Policy']")?;
    let tag = iter.into_iter().next()?.get(dom.parser())?.as_tag()?;
    let (start, _) = tag.boundaries(dom.parser());
    let len = tag.raw().as_utf8_str().len();
    Some(TextRange::new(start, start + len))
}

// ---- Text content extraction ----

/// Strip HTML tags and decode entities, similar to JS `textContent`.
fn text_content(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_html_entities(&text)
}

fn decode_html_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let mut remainder = text;

    while let Some(amp_pos) = remainder.find('&') {
        result.push_str(&remainder[..amp_pos]);
        remainder = &remainder[amp_pos..];

        // Scan bytes (not a substring slice) so we don't panic when the
        // 32-byte cap lands inside a multibyte UTF-8 char. `;` is ASCII, so
        // any byte position we find is a valid string index.
        let search_end = remainder.len().min(32);
        let semi_pos = remainder.as_bytes()[..search_end]
            .iter()
            .position(|&b| b == b';');
        if let Some(semi_pos) = semi_pos {
            let entity = &remainder[1..semi_pos]; // between & and ;
            if let Some(c) = resolve_entity(entity) {
                result.push(c);
            } else {
                result.push_str(&remainder[..semi_pos + 1]);
            }
            remainder = &remainder[semi_pos + 1..];
        } else {
            result.push('&');
            remainder = &remainder[1..];
        }
    }

    result.push_str(remainder);
    result
}

fn resolve_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        s if s.starts_with("#x") || s.starts_with("#X") => {
            u32::from_str_radix(&s[2..], 16)
                .ok()
                .and_then(char::from_u32)
        }
        s if s.starts_with('#') => s[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}
