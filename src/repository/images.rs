use super::db_manager::{
    AdminImageResource, ImageSource, NewReceivedImage, QQChatContextManager, ReceivedImageRecord,
};
use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

impl QQChatContextManager {
    /// 根据图片内容哈希查询已接收图片，用于重复图片复用描述。
    pub fn get_received_image_by_hash(
        &self,
        source: ImageSource,
        content_hash: &str,
    ) -> Result<Option<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let record = connection
            .query_row(
                "
                SELECT image_id, content_hash, local_path, description
                FROM received_images
                WHERE source = ?1 AND content_hash = ?2
                ",
                params![source.as_str(), content_hash],
                |row| {
                    Ok(ReceivedImageRecord {
                        image_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        local_path: row.get(2)?,
                        description: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    /// 根据图片短 ID 查询已接收图片，供后台描述和主模型图像上下文读取原图。
    pub fn get_received_image_by_id(&self, image_id: &str) -> Result<Option<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let record = connection
            .query_row(
                "
                SELECT image_id, content_hash, local_path, description
                FROM received_images
                WHERE image_id = ?1
                ",
                params![image_id],
                |row| {
                    Ok(ReceivedImageRecord {
                        image_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        local_path: row.get(2)?,
                        description: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn get_admin_image_resource(&self, image_id: &str) -> Result<Option<AdminImageResource>> {
        self.conn_pool
            .get()?
            .query_row(
                "SELECT local_path, mime_type FROM received_images WHERE image_id = ?1",
                params![image_id],
                |row| {
                    Ok(AdminImageResource {
                        local_path: row.get(0)?,
                        mime_type: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 判断图片短 ID 是否已经存在，避免随机 ID 碰撞。
    pub fn received_image_id_exists(&self, image_id: &str) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let exists: i64 = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM received_images WHERE image_id = ?1)",
            params![image_id],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// 写入一张新接收图片的索引记录。
    pub fn insert_received_image(&self, image: &NewReceivedImage) -> Result<()> {
        let now_timestamp = Utc::now().timestamp();
        let connection = self.conn_pool.get()?;
        connection.execute(
            "
            INSERT INTO received_images (
                image_id,
                content_hash,
                local_path,
                original_url,
                mime_type,
                file_size,
                description,
                metadata_json,
                created_at,
                updated_at,
                source,
                last_used_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?10)
            ",
            params![
                &image.image_id,
                &image.content_hash,
                &image.local_path,
                Self::normalize_optional_text(image.original_url.as_deref()),
                Self::normalize_optional_text(image.mime_type.as_deref()),
                image.file_size,
                &image.description,
                &image.metadata_json,
                now_timestamp,
                now_timestamp,
                image.source.as_str(),
            ],
        )?;
        Ok(())
    }

    /// 完成后台图片描述，并同步更新所有已经引用该图片的消息正文与结构化片段。
    pub fn complete_received_image_description(
        &self,
        image_id: &str,
        description: &str,
        placeholder_text: &str,
        described_text: &str,
    ) -> Result<usize> {
        let now_timestamp = Utc::now().timestamp();
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;
        let image_changed = tx.execute(
            "
            UPDATE received_images
            SET description = ?2, updated_at = ?3
            WHERE image_id = ?1
            ",
            params![image_id, description, now_timestamp],
        )?;
        if image_changed == 0 {
            tx.commit()?;
            return Ok(0);
        }

        let local_path: String = tx.query_row(
            "SELECT local_path FROM received_images WHERE image_id = ?1",
            params![image_id],
            |row| row.get(0),
        )?;
        // 先筛出可能引用该图片的消息，再用 JSON 结构确认并修改对应图片片段。
        let candidates = {
            let mut statement = tx.prepare(
                "
                SELECT id, content_text, content_parts_json
                FROM messages
                WHERE instr(content_parts_json, ?1) > 0
                ",
            )?;
            let rows = statement
                .query_map(params![image_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let mut updated_messages = 0usize;
        for (message_id, content_text, content_parts_json) in candidates {
            let mut parts: Value = serde_json::from_str(&content_parts_json)?;
            let Some(parts) = parts.as_array_mut() else {
                continue;
            };
            let mut references_image = false;
            let mut context_replacements = Vec::new();
            for part in &mut *parts {
                if part.get("kind").and_then(Value::as_str) != Some("image") {
                    continue;
                }
                let Some(data) = part.get_mut("data").and_then(Value::as_object_mut) else {
                    continue;
                };
                if data.get("image_id").and_then(Value::as_str) != Some(image_id) {
                    continue;
                }
                let previous_context = data
                    .get("context_text")
                    .and_then(Value::as_str)
                    .unwrap_or(placeholder_text)
                    .to_string();
                let next_context = Self::image_context_with_description(
                    &previous_context,
                    placeholder_text,
                    described_text,
                );
                data.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
                data.insert("local_path".to_string(), Value::String(local_path.clone()));
                data.insert(
                    "context_text".to_string(),
                    Value::String(next_context.clone()),
                );
                context_replacements.push((previous_context, next_context));
                references_image = true;
            }
            if !references_image {
                continue;
            }

            let content_text = content_text.map(|mut text| {
                for (previous_context, next_context) in context_replacements {
                    text = text.replace(&previous_context, &next_context);
                }
                text
            });
            let content_parts_json = serde_json::to_string(parts)?;
            tx.execute(
                "
                UPDATE messages
                SET content_text = ?2, content_parts_json = ?3
                WHERE id = ?1
                ",
                params![message_id, content_text, content_parts_json],
            )?;
            updated_messages += 1;
        }

        tx.commit()?;
        Ok(updated_messages)
    }

    /// 只替换内部图片占位，保留文件图片的外层文件信息。
    fn image_context_with_description(
        previous_context: &str,
        image_placeholder: &str,
        described_text: &str,
    ) -> String {
        if previous_context.contains(image_placeholder) {
            previous_context.replace(image_placeholder, described_text)
        } else {
            described_text.to_string()
        }
    }

    /// 刷新图片使用时间和可用文件路径，保留图片 ID 与描述。
    pub fn touch_image(&self, image_id: &str, local_path: &str) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "UPDATE received_images SET local_path = ?2, last_used_at = ?3 WHERE image_id = ?1",
            params![image_id, local_path, Utc::now().timestamp()],
        )?;
        Ok(changed > 0)
    }

    /// 查询最近使用时间早于截止时间的接收图片。
    pub fn get_received_images_created_before(
        &self,
        cutoff_timestamp: i64,
    ) -> Result<Vec<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            SELECT image_id, content_hash, local_path, description
            FROM received_images
            WHERE source = 'received' AND last_used_at < ?1
            ORDER BY last_used_at ASC, id ASC
            ",
        )?;
        let images = statement
            .query_map(params![cutoff_timestamp], |row| {
                Ok(ReceivedImageRecord {
                    image_id: row.get(0)?,
                    content_hash: row.get(1)?,
                    local_path: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(images)
    }

    /// 在写事务内复核使用时间并删除文件，避免清理已重新使用的图片。
    pub fn delete_expired_received_image(
        &self,
        image_id: &str,
        cutoff_timestamp: i64,
        remove_file: impl FnOnce(&str) -> Result<()>,
    ) -> Result<bool> {
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let path: Option<String> = tx.query_row(
            "SELECT local_path FROM received_images WHERE image_id = ?1 AND source = 'received' AND last_used_at < ?2",
            params![image_id, cutoff_timestamp],
            |row| row.get(0),
        ).optional()?;
        let Some(path) = path else {
            return Ok(false);
        };
        remove_file(&path)?;
        tx.execute(
            "DELETE FROM received_images WHERE image_id = ?1",
            params![image_id],
        )?;
        tx.commit()?;
        Ok(true)
    }
}
