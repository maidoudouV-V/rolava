---
name: bilibili-video-reader
description: 读取B站视频字幕，用于基于视频内容的总结、问答和分析；不用于下载视频、读取弹幕或评论。
metadata:
  openclaw:
    requires:
      bins:
        - python
        - yt-dlp
---

# B站视频字幕读取 Skill

## 配置

从 `BILIBILI_COOKIE` 环境变量读取完整的B站登录 Cookie。缺失或失效时提醒用户更新该环境变量，不要索要 API Key，也不要输出 Cookie。

## 调用

```bash
python scripts/bilibili-subtitle.py --url="https://www.bilibili.com/video/BV..." --part=1
```

`--url` 接受B站视频链接、`b23.tv` 短链接或 BV 号。用户指定分P时传 `--part`；否则读取链接指定分P或 P1。

## 使用原则

- 用户要求读取、总结或分析B站视频时调用字幕脚本。
- 只根据成功取得的字幕回答，不把标题、简介、评论或弹幕当作视频正文。
- `truncated=true` 表示只取得字幕首尾，不得声称已经完整阅读或总结整个视频。
- 并非每个视频都有字幕；`NO_SUBTITLE` 时如实说明，`LOGIN_REQUIRED` 时提示更新 `BILIBILI_COOKIE`。
