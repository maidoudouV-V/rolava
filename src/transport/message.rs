use serde_json::Value;

/// transport 层统一入站事件。
/// 外部平台的原始协议先转换成这个结构，再进入后续业务流程。
#[derive(Debug, Clone)]
pub enum IncomingEvent {
    /// 聊天消息事件。
    Message(IncomingMessage),
    /// 系统或平台事件，例如心跳、连接、生命周期变化。
    System(IncomingSystemEvent),
}

/// 通用入站消息。
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// 消息来源平台，例如 `onebot`。
    pub source: String,
    /// 当前接收消息的机器人账号 ID。
    pub bot_id: String,
    /// 这条消息所属的会话。
    pub conversation: Conversation,
    /// 消息发送者。
    pub sender: Participant,
    /// 消息内容。
    pub content: MessageContent,
    /// 平台上的原始消息 ID。
    pub message_id: Option<String>,
    /// 事件发生时间戳，单位秒。
    pub timestamp: i64,
    /// 平台相关但暂时未标准化的扩展字段。
    pub metadata: Value,
}

/// 通用系统事件。
#[derive(Debug, Clone)]
pub struct IncomingSystemEvent {

}

/// 消息所属会话。
#[derive(Debug, Clone)]
pub struct Conversation {
    /// 会话唯一 ID。
    /// 私聊一般对应对方用户 ID，群聊一般对应群 ID。
    pub id: String,
    /// 会话类型。
    pub kind: ConversationKind,
    /// 会话展示名称。
    pub title: Option<String>,
}

/// 会话类型。
#[derive(Debug, Clone)]
pub enum ConversationKind {
    /// 一对一会话。
    Direct,
    /// 群聊会话。
    Group,
    /// 频道、话题或其它公开会话。
    Channel,
}

/// 会话参与者。
#[derive(Debug, Clone)]
pub struct Participant {
    /// 参与者唯一 ID。
    pub id: String,
    /// 参与者展示名称。
    pub display_name: String,
    /// 参与者在当前会话中的昵称或备注。
    pub nickname: Option<String>,
    /// 参与者角色，例如 `owner`、`admin`、`member`。
    pub role: Option<String>,
}

/// 标准化后的消息内容。
#[derive(Debug, Clone)]
pub struct MessageContent {
    /// 可直接用于大多数文本流程的纯文本内容。
    pub text: String,
    /// 保留结构化消息片段，便于处理图片、提及、回复等富内容。
    pub parts: Vec<MessagePart>,
}

/// 标准化后的消息片段。
#[derive(Debug, Clone)]
pub struct MessagePart {
    /// 片段类型，例如 `text`、`image`、`mention`、`reply`。
    pub kind: String,
    /// 片段的结构化数据。
    pub data: Value,
}
