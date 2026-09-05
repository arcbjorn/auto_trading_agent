//! A small Markdown renderer for the result reports: headings, paragraphs, bullet lists, tables,
//! inline code, bold and links. Everything is escaped; the reports are written by this
//! repository's own tools, but the renderer does not rely on that.

use super::html::esc;

/// Inline spans: `code`, **bold**, [text](url). Anything else is escaped text.
fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str(&format!("<code>{}</code>", esc(&after[..end])));
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                out.push_str(&format!("<b>{}</b>", inline(&after[..end])));
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('[') {
            if let (Some(close), Some(paren)) = (after.find("]("), after.find(')')) {
                if close < paren {
                    let text = &after[..close];
                    let url = &after[close + 2..paren];
                    if url.starts_with("http://") || url.starts_with("https://") {
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
                            esc(url),
                            inline(text)
                        ));
                    } else {
                        // A repository-relative link has no target on this page: keep its text.
                        out.push_str(&format!("{} <code class=\"muted\">{}</code>", inline(text), esc(url)));
                    }
                    rest = &after[paren + 1..];
                    continue;
                }
            }
        }
        let mut chars = rest.char_indices();
        let (_, c) = chars.next().expect("non-empty");
        let next = chars.next().map_or(rest.len(), |(i, _)| i);
        out.push_str(&esc(&c.to_string()));
        rest = &rest[next..];
    }
    out
}

fn table_cells(line: &str) -> Vec<&str> {
    line.trim().trim_matches('|').split('|').map(str::trim).collect()
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

pub fn render(md: &str) -> String {
    let mut out = String::from("<div class=\"md\">");
    let mut lines = md.lines().peekable();
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, out: &mut String| {
        if !paragraph.is_empty() {
            out.push_str(&format!("<p>{}</p>", inline(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    while let Some(line) = lines.next() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            flush(&mut paragraph, &mut out);
        } else if let Some(h) = t.strip_prefix("### ") {
            flush(&mut paragraph, &mut out);
            out.push_str(&format!("<h3>{}</h3>", inline(h)));
        } else if let Some(h) = t.strip_prefix("## ") {
            flush(&mut paragraph, &mut out);
            out.push_str(&format!("<h2>{}</h2>", inline(h)));
        } else if let Some(h) = t.strip_prefix("# ") {
            flush(&mut paragraph, &mut out);
            out.push_str(&format!("<h1>{}</h1>", inline(h)));
        } else if t.starts_with("```") {
            flush(&mut paragraph, &mut out);
            let mut code = Vec::new();
            for l in lines.by_ref() {
                if l.starts_with("```") {
                    break;
                }
                code.push(l);
            }
            out.push_str(&format!("<pre>{}</pre>", esc(&code.join("\n"))));
        } else if t.starts_with("* ") || t.starts_with("- ") {
            flush(&mut paragraph, &mut out);
            // Items are buffered because a wrapped bullet continues on indented lines.
            let mut items: Vec<String> = vec![t[2..].to_string()];
            while let Some(next) = lines.peek() {
                let n = next.trim_end();
                if n.starts_with("* ") || n.starts_with("- ") {
                    items.push(n[2..].to_string());
                    lines.next();
                } else if n.starts_with("  ") && !n.trim().is_empty() {
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(n.trim());
                    }
                    lines.next();
                } else {
                    break;
                }
            }
            out.push_str("<ul>");
            for item in &items {
                out.push_str(&format!("<li>{}</li>", inline(item)));
            }
            out.push_str("</ul>");
        } else if t.trim_start().starts_with('|') {
            flush(&mut paragraph, &mut out);
            out.push_str("<div class=\"scroll\"><table>");
            let header = table_cells(t);
            let has_separator = lines.peek().is_some_and(|n| is_separator(n));
            if has_separator {
                lines.next();
                out.push_str("<thead><tr>");
                for c in &header {
                    out.push_str(&format!("<th class=\"l\">{}</th>", inline(c)));
                }
                out.push_str("</tr></thead>");
            }
            out.push_str("<tbody>");
            if !has_separator {
                out.push_str("<tr>");
                for c in &header {
                    out.push_str(&format!("<td class=\"l\">{}</td>", inline(c)));
                }
                out.push_str("</tr>");
            }
            while let Some(next) = lines.peek() {
                if !next.trim_start().starts_with('|') {
                    break;
                }
                let row = lines.next().expect("peeked");
                if is_separator(row) {
                    continue;
                }
                out.push_str("<tr>");
                for c in table_cells(row) {
                    out.push_str(&format!("<td class=\"l\">{}</td>", inline(c)));
                }
                out.push_str("</tr>");
            }
            out.push_str("</tbody></table></div>");
        } else {
            paragraph.push(t.trim());
        }
    }
    flush(&mut paragraph, &mut out);
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_the_report_shapes_and_escapes() {
        let md = "# Title\n\nA line with `code`, **bold** and a [link](https://example.com).\nSame paragraph.\n\n| a | b |\n|---|---|\n| 1 | <x> |\n\n* one\n* two\n  continued\n\n```\nlet x = 1 < 2;\n```\n";
        let html = render(md);
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<code>code</code>") && html.contains("<b>bold</b>"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("<p>A line with") && html.contains("Same paragraph.</p>"));
        assert!(html.contains("<th class=\"l\">a</th>") && html.contains("<td class=\"l\">&lt;x&gt;</td>"));
        assert!(html.contains("<li>two continued</li>"));
        assert!(html.contains("<pre>let x = 1 &lt; 2;</pre>"));
        assert!(!html.contains("<x>"), "raw angle brackets never reach the page");
    }

    #[test]
    fn relative_links_keep_their_text() {
        let html = render("See the [engine doc](../02-engine.md) and <script>alert(1)</script>.");
        assert!(html.contains("engine doc <code class=\"muted\">../02-engine.md</code>"));
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>"));
    }
}
