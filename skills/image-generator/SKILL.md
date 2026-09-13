---
name: image-generator
description: 根据文字生成图片，或使用已有图片进行编辑；适用于用户要求创作、修改或合成图片时。
metadata:
  openclaw:
    requires:
      bins:
        - node
---

# 图片生成与编辑 Skill

## 配置

从 `IMAGE_GENERATION_BASE_URL`、`IMAGE_GENERATION_MODEL_NAME` 和 `IMAGE_GENERATION_API_KEY` 环境变量读取接口配置，不要输出 API Key。

## 调用

文字生图：

```bash
node scripts/image-generate.js --prompt="图片描述"
```

编辑图片时重复传入一个或多个已有路径：

```bash
node scripts/image-generate.js --prompt="编辑要求" --image="/data/received_images/a.png" --image="/data/images/b.png"
```

每次调用只生成或编辑一张图片。输入图片必须来自 `/data/images/` 或 `/data/received_images/`。成功后使用返回的 `paths`；若返回 `text`，一并参考该说明。
