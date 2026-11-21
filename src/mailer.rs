//! SMTP 邮件发送（后台「系统设置 → 邮箱服务」配置）。
//! 配置缺失/未启用时返回 Err，调用方据此给用户明确提示。

use crate::store::store;
use crate::util;
use serde_json::{json, Value};
use std::time::Duration;

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub from: String,
    pub tls: bool,
}

pub fn smtp_config() -> Result<SmtpConfig, String> {
    let cfg = store().load();
    let c = cfg.get("config").cloned().unwrap_or(json!({}));
    let host = util::str_or(c.get("smtp_host"), "").trim().to_string();
    if host.is_empty() {
        return Err("管理员尚未配置 SMTP 服务器".into());
    }
    let port = util::int_or(c.get("smtp_port"), 465).clamp(1, 65535) as u16;
    let user = util::str_or(c.get("smtp_user"), "").trim().to_string();
    let pass = util::str_or(c.get("smtp_pass"), "");
    let from_field = util::str_or(c.get("smtp_from"), "").trim().to_string();
    let from = if from_field.is_empty() { user.clone() } else { from_field };
    let tls = util::truthy(c.get("smtp_tls").unwrap_or(&Value::Bool(true)));
    Ok(SmtpConfig { host, port, user, pass, from, tls })
}

/// 发送一封文本邮件。to_addr 收件人；subject 主题；body 纯文本正文。
pub async fn send_mail(to_addr: &str, subject: &str, body: &str) -> Result<(), String> {
    use lettre::message::header::ContentType;
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let cfg = smtp_config()?;
    let from_addr: Mailbox = cfg
        .from
        .parse()
        .map_err(|_| "SMTP 发件地址格式无效".to_string())?;
    let to_box: Mailbox = to_addr
        .parse()
        .map_err(|_| "收件邮箱格式无效".to_string())?;

    let email = Message::builder()
        .from(from_addr)
        .to(to_box)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("邮件构建失败: {e}"))?;

    let creds = Credentials::new(cfg.user.clone(), cfg.pass.clone());
    let builder = if cfg.tls {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
            .map_err(|e| format!("SMTP 连接配置失败: {e}"))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
    }
    .port(cfg.port)
    .credentials(creds)
    .timeout(Some(Duration::from_secs(20)));
    let mailer = builder.build();
    mailer
        .send(email)
        .await
        .map(|_| ())
        .map_err(|e| format!("邮件发送失败: {e}"))
}


