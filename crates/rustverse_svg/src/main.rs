use regex::Regex;
use resvg::{tiny_skia, usvg};
use rustverse_svg::*;
use serde::Serialize;

pub fn render_from_serialize<T: Serialize>(template: &str, data: &T) -> Vec<u8> {
    let mut env = MJ_ENVIRONMENT.clone();
    env.add_filter("game_text", format_game_text);
    env.add_filter("split", split_filter);
    env.add_filter("wrap_game_text", wrap_game_text);
    env.add_filter("strip_all_tags", strip_all_tags_filter);
    env.add_filter("element_filter", element_filter);
    let template = env.template_from_str(template).unwrap();
    let rendered = template.render(data).unwrap();

    std::fs::write("rendered.svg", &rendered).unwrap();

    let tree = usvg::Tree::from_data(rendered.as_bytes(), &USVG_OPTIONS).unwrap();
    let pixmap_size = tree.size().to_int_size().scale_by(ZOOM_FACTOR).unwrap();
    let mut pixmap = tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height()).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(ZOOM_FACTOR, ZOOM_FACTOR),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().unwrap()
}

fn element_filter(_state: &minijinja::State, val: minijinja::Value, target: i64) -> Vec<String> {
    let mut result = Vec::new();

    if let Some(map) =
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&val).unwrap())
            .unwrap()
            .as_object()
    {
        for key in map.keys() {
            if let Some(v) = map.get(key)
                && v.as_i64() == Some(target)
            {
                let key_str = key.to_string();
                let capitalized = format!("{}{}", key_str[0..1].to_uppercase(), &key_str[1..]);
                result.push(capitalized);
            }
        }
    }
    result.sort();
    result
}

fn is_prohibited_start(c: char) -> bool {
    matches!(
        c,
        // Standard ASCII punctuation that should not start a line
        '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '”' | '’' |
        // Common CJK punctuation that should not start a line (Kinsoku Shori)
        '。' | '、' | '！' | '？' | '：' | '；' | '）' | '】' | '』' | '」'
    )
}

struct CharItem {
    c: char,
    is_visible: bool,
}

struct Token {
    text: String,
    visible_len: usize,
    visible_len_no_trailing_space: usize,
}

pub fn wrap_game_text(_state: &minijinja::State, text: String, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        // Step 1: Flatten paragraph into characters, marking which are visually rendered
        let mut items = Vec::new();
        let mut in_tag = false;

        for c in paragraph.chars() {
            if c == '<' {
                in_tag = true;
                items.push(CharItem {
                    c,
                    is_visible: false,
                });
            } else if c == '>' && in_tag {
                in_tag = false;
                items.push(CharItem {
                    c,
                    is_visible: false,
                });
            } else {
                items.push(CharItem {
                    c,
                    is_visible: !in_tag,
                });
            }
        }

        // Step 2: Group characters into indivisible word tokens
        let mut tokens = Vec::new();
        let mut current_text = String::new();
        let mut current_visible_len = 0;
        let mut current_visible_len_no_space = 0;

        let mut i = 0;
        while i < items.len() {
            let item = &items[i];
            current_text.push(item.c);

            if item.is_visible {
                current_visible_len += 1;
                if !item.c.is_whitespace() {
                    current_visible_len_no_space = current_visible_len;
                }
            }

            // If we hit a visible space, we decide if we are allowed to break the token here
            if item.is_visible && item.c.is_whitespace() {
                let mut can_break = false;
                let mut found_next_visible = false;
                let mut next_visible_char = ' ';

                // Look ahead to find the NEXT visible character
                for next_item in items.iter().skip(i + 1) {
                    if next_item.is_visible {
                        found_next_visible = true;
                        next_visible_char = next_item.c;
                        break;
                    }
                }

                if found_next_visible {
                    // We only break if the next visible character is a normal letter/number
                    // (not another space, and NOT a hanging punctuation)
                    if !next_visible_char.is_whitespace() && !is_prohibited_start(next_visible_char)
                    {
                        can_break = true;
                    }
                }

                if can_break {
                    tokens.push(Token {
                        text: std::mem::take(&mut current_text), // efficiently transfers the string memory
                        visible_len: current_visible_len,
                        visible_len_no_trailing_space: current_visible_len_no_space,
                    });
                    current_visible_len = 0;
                    current_visible_len_no_space = 0;
                }
            }
            i += 1;
        }

        // Flush remaining text into the last token
        if !current_text.is_empty() {
            tokens.push(Token {
                text: current_text,
                visible_len: current_visible_len,
                visible_len_no_trailing_space: current_visible_len_no_space,
            });
        }

        // Step 3: Pack tokens into lines
        let mut current_line = String::new();
        let mut current_line_vis_len = 0;

        for token in tokens {
            // We use visible_len_no_trailing_space for max_width comparison so that
            // trailing spaces at the end of a line don't prematurely force a wrap.
            if current_line_vis_len + token.visible_len_no_trailing_space > max_width
                && !current_line.is_empty()
            {
                lines.push(current_line.trim_end().to_string());
                current_line.clear();
                current_line_vis_len = 0;
            }

            current_line.push_str(&token.text);
            current_line_vis_len += token.visible_len;
        }

        // Flush any remaining characters in the line buffer
        if !current_line.is_empty() {
            lines.push(current_line.trim_end().to_string());
        }
    }

    lines
}

fn split_filter(_state: &minijinja::State, value: String, delimiter: String) -> Vec<String> {
    value.split(&delimiter).map(|s| s.to_string()).collect()
}

fn format_game_text(_state: &minijinja::State, value: String) -> Result<String, minijinja::Error> {
    let mut s = value.replace("</color>", "</tspan>");
    // while let Some(pos) = s.find("<IconMap") {
    //     let end = s[pos..].find(">").unwrap();
    //     s = s.replace(&s[pos..=end], "");
    // }
    s = s.replace("<color=", r#"<tspan fill=""#);
    s = s.replace(">", r#"">"#);
    s = s.replace(r#"</tspan">"#, "</tspan>");

    Ok(s)
}

fn strip_all_tags_filter(_state: &minijinja::State, value: String) -> String {
    let re_term = Regex::new(r"<Term[^>]*>").unwrap();
    let re_icon = Regex::new(r"<IconMap[^>]*>").unwrap();

    let without_terms = re_term.replace_all(&value, "");
    re_icon
        .replace_all(without_terms.as_ref(), "")
        .to_string()
        .replace("</Term>", "")
}

fn main() {
    let in_file = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(std::env::args().nth(2).unwrap()).unwrap())
            .unwrap();

    std::fs::write(
        std::env::args().nth(1).unwrap().replace("j2", "png"),
        render_from_serialize(&in_file, &data),
    )
    .unwrap();
}
