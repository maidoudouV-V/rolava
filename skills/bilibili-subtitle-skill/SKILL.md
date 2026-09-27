---
name: bilibili-subtitle
description: 读取B站视频字幕，用于基于视频内容的总结、问答和分析；不用于下载视频、弹幕或评论。
metadata:
  openclaw:
    requires:
      bins:
        - node
---

# B站视频字幕

## 调用

```bash
node subtitle.mjs --url="B站链接、短链接或BV号" --part=1
```

用户指定分P时传 `--part`；否则读取链接指定分P或 P1。WEB 登录凭据由管理页维护，不要向用户索要 Cookie 或 API Key。

只根据成功取得的字幕回答，不把标题、简介、评论或弹幕当作正文。并非每个视频都有字幕；`truncated=true` 时只取得了字幕首尾，不得声称完整阅读或总结了整个视频。
