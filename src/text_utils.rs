const MAX_TEXT_CHARS: usize = 10_000;
const RETAINED_EDGE_CHARS: usize = 5_000;

/// 超过一万字符时截断中间部分，保留文本开头和结尾各五千字符。
pub fn truncate_long_text(text: &str) -> String {
    let total_chars = text.chars().count();
    if total_chars <= MAX_TEXT_CHARS {
        return text.to_string();
    }

    // 按 Unicode 字符而不是 UTF-8 字节截取，避免切断中文等多字节字符。
    let head = text.chars().take(RETAINED_EDGE_CHARS).collect::<String>();
    let tail = text
        .chars()
        .skip(total_chars - RETAINED_EDGE_CHARS)
        .collect::<String>();
    let truncated_chars = total_chars - RETAINED_EDGE_CHARS * 2;
    format!("{head}\n……已截断{truncated_chars}个字符……\n{tail}")
}
