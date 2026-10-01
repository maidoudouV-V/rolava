# Rolava

Rolava 是一个使用 Rust 开发的 QQ 角色扮演 AI 机器人，通过 OneBot HTTP 接入群聊和私聊。

它希望让 AI 角色自然地参与聊天：结合角色设定、聊天上下文和记忆判断何时回复，不必每次都通过 @ 或命令唤醒。你可以自定义角色，也可以通过工具和本地 Skills 扩展它的能力。

## 主要功能

- **角色与对话**：自定义角色设定和回复规则，支持群聊、私聊及独立的上下文长度配置。
- **前置过滤**：可使用单独的小模型判断消息是否需要回复，减少主模型处理无关消息的开销。
- **上下文与记忆**：保存聊天记录，支持历史摘要、用户记忆和群记忆。
- **工具与扩展**：支持图片理解、联网搜索、定时任务和本地 Skills。
- **多模型接入**：支持 OpenAI Compatible、OpenAI Responses、OpenRouter 和 Gemini，可为不同任务配置模型。
- **Web 管理**：在浏览器中管理模型、提示词、Skills 和会话，查看运行状态与日志。

各项能力可按需配置，部分功能支持单独关闭。

## Docker 部署

目前主要使用 Docker 部署。开始前需要准备：

- Docker 和 Docker Compose
- 一个支持 HTTP API 与事件上报的 OneBot 实现，例如 NapCatQQ
- 可用的 AI 模型 API

下载或克隆本仓库，在项目根目录创建 `compose.yaml`。下方配置依赖仓库中的 `config/`、`prompt/` 和 `skills/`，请保留这些目录及其内容。

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
      - ./skills:/app/skills
      - ./data:/app/data
```

启动服务：

```bash
docker compose up -d
```

访问 `http://<服务器地址>:8080/admin/` 打开管理页面。`config/meta.toml` 中的 `[admin].token` 为空时，启动会自动生成并写入管理 Token，使用它登录即可。

登录后，配置 OneBot API 地址、模型服务商、API Key、使用的模型和允许响应的群聊／私聊，再调整角色提示词。OneBot 的 HTTP 事件上报地址应指向 Rolava 的根路径，例如 `http://<Rolava 地址>:8080/`，并确保两端网络可达、鉴权配置一致。


## 当前状态

项目仍处于开发阶段，功能和配置格式可能调整，目前不保证兼容所有 OneBot 实现和模型接口。

## 相关项目

- [NapCatQQ](https://github.com/NapNeko/NapCatQQ)：QQ 机器人框架。
- [LangBot（原 QChatGPT）](https://github.com/langbot-app/LangBot)、[MaiBot](https://github.com/Mai-with-u/MaiBot)：项目的灵感来源。
