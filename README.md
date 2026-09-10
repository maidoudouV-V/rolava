# Rolava

Rolava 是一个基于 Rust 开发的 QQ 角色扮演 AI 机器人。

项目通过 OneBotHTTP 协议控制机器人账号。目前仍处于初版阶段，整体结构和功能可能继续调整。

## 主要功能

- 无感对话：在群内直接唤醒AI角色，无需@指定角色
- 节约token：拥有前置过滤功能，使用小模型过滤无用对话，主模型只负责回复有意义消息
- 通过 OneBot HTTP 接入 QQ 私聊和群聊
- 支持 OpenAI Compatible、OpenAI Responses、OpenRouter 和 Gemini 协议
- 保存聊天记录，并提供记忆、历史摘要和定时任务等能力
- 支持图片理解、联网搜索和本地 Skills 扩展
- 自带一个简单的管理页面，用来调整配置、查看会话和运行日志

部分能力可以按需关闭。

## 项目结构

```text
src/       Rust 后端与机器人逻辑
prompt/    角色和提示词
skills/    本地 Skills
web/       管理页面
config/    初始化配置与表情映射
data/      运行时数据
```

## Docker 部署

目前开发阶段只验证过 Docker 部署方式。开始前需要准备：

- 一个支持 HTTP 通信的 OneBot 实现
- 可用的 AI 模型 API

### DockerCompose运行方式:

```yaml
services:
  rolava:
    image: ghcr.io/maidoudouv-v/rolava:dev
    container_name: rolava
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - ./config:/app/config
      - ./prompt:/app/prompt
      - ./data:/app/data
```

`data/` 用于保存运行数据。首次启动时会自动创建 `data/test_chat.db` 并初始化数据库表。

启动后可以访问：

- 管理页面：`http://<服务器地址>:8080/admin/`

首次启动会自动生成管理 Token，并写入 `config/meta.toml`。使用该 Token 登录后，可以在管理页面中完成 OneBot、模型、消息行为和其他功能的配置。

## 当前状态

Rolava 正在开发阶段，个人开发者精力有限，不保证兼容所有 OneBot 实现和模型接口。配置格式与功能仍可能继续变化。

## 相关项目
### QQ机器人框架

- [NapCatQQ](https://github.com/NapNeko/NapCatQQ)

### 灵感来源

- [LangBot(原QChatGPT)](https://github.com/langbot-app/LangBot)
- [MaiBot](https://github.com/Mai-with-u/MaiBot)