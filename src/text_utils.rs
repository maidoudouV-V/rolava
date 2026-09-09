const MAX_TEXT_CHARS: usize = 20_000;

/// 超过两万字符时截断中间部分，并平均保留文本开头和结尾。
pub fn truncate_long_text(text: &str) -> String {
    let total_chars = text.chars().count();
    if total_chars <= MAX_TEXT_CHARS {
        return text.to_string();
    }

    // 按 Unicode 字符而不是 UTF-8 字节截取，避免切断中文等多字节字符。
    let tail_chars = MAX_TEXT_CHARS / 2;
    let head_chars = MAX_TEXT_CHARS - tail_chars;
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .skip(total_chars - tail_chars)
        .collect::<String>();
    let truncated_chars = total_chars - head_chars - tail_chars;
    format!("{head}\n……已截断{truncated_chars}个字符……\n{tail}")
}
