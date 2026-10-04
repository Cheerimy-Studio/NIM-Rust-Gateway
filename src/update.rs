//! 远程更新与回滚（server.py 更新机制的 Rust 版）。
//!
//! Python 版下载的是「源码 tarball + py_compile 自检 + execv 重启」；Rust 是编译型语言，
//! 等价实现为「下载包含平台二进制的更新包 → 校验 → 备份 → 原子替换可执行文件 → 自重启」。
//! HTTP 契约（/api/update、/api/rollback 的返回与备份/回滚语义）保持不变。
//!
//! 更新源优先级：
//!   1. `NGW_UPDATE_URL`（显式指定，始终安装指向的内容，不做版本比较）
//!   2. GitHub Releases（默认）：取 `NIM-Rust-Gateway` 的最新「正式 Release」
//!      （/releases/latest 天然排除 draft 与 prerelease），按当前平台挑选资产、
//!      与运行版本比较——已是最新则不下载不重启。只发正式版才会被线上收到。

use crate::store::store;
use crate::util;
use serde_json::Value;
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &["data", "tests", "backup", "_update_tmp", ".git", "target"];

const DEFAULT_REPO: &str = "Cheerimy-Studio/NIM-Rust-Gateway";

// 更新/回滚互斥：两个安装流程并发共用 _update_tmp 会互相拆台（解包报路径不存在）。
// 触发源有手动端点与定时自动检查两条，必须串行。
static UPDATE_IN_PROGRESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 进程启动时捕获的可执行文件路径。
///
/// 运行中二进制被替换/删除后（面板覆盖部署、上一次更新换过文件），
/// Linux 的 /proc/self/exe 会变成 "... (deleted)" 且原路径可能已不存在 ——
/// 更新/重启若在那一刻重新解析路径，就会「备份当前二进制」失败把更新卡死。
/// 启动时路径一定存在，以它为准。
static EXE_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_exe_path(p: PathBuf) {
    let _ = EXE_PATH.set(p);
}

pub fn exe_path() -> Option<PathBuf> {
    if let Some(p) = EXE_PATH.get() {
        return Some(p.clone());
    }
    let raw = std::env::current_exe().ok()?;
    let s = raw.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_suffix(" (deleted)") {
        return Some(PathBuf::from(stripped));
    }
    Some(raw)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("nim-gateway")
        .build()
        .unwrap_or_default()
}

async fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    // 到 GitHub 的链路常见瞬断（连接重置 / 响应体截断），自动重试一次
    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let client = http_client();
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last = format!("下载失败: {}", e),
            },
            Ok(r) if r.status() == reqwest::StatusCode::FORBIDDEN || r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                return Err("更新源限流（HTTP 403/429），请稍后重试或改用自建更新源".into());
            }
            Ok(r) => return Err(format!("更新源返回 HTTP {}", r.status())),
            Err(e) => last = format!("下载失败: {}", e),
        }
    }
    Err(last)
}

/// 解析 "v1.7.0" / "1.7.0-rc1" 形态的版本号；不可解析返回 None。
fn parse_version(s: &str) -> Option<Vec<u64>> {
    let t = s.trim().trim_start_matches(['v', 'V']);
    let core = t.split(['-', '+']).next()?.trim();
    let parts: Vec<u64> = core
        .split('.')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() { None } else { Some(parts) }
}

/// a >= b（按段比较，缺段补 0）。
fn version_ge(a: &[u64], b: &[u64]) -> bool {
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

/// 轻量检查：有比当前版本新的正式 Release 时返回 (tag, note 尚未下载)。
/// 供定时自动检查用；不做下载与安装。
pub async fn check_newer_tag() -> Result<Option<String>, String> {
    let api_base = std::env::var("NGW_UPDATE_API_BASE")
        .unwrap_or_else(|_| "https://api.github.com".into())
        .trim_end_matches('/')
        .to_string();
    let repo = std::env::var("NGW_UPDATE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let api = format!("{}/repos/{}/releases/latest", api_base, repo);
    let client = http_client();
    let rel: Value = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 Release 失败: {}", e))?
        .error_for_status()
        .map_err(|e| format!("查询 Release 失败: {}", e))?
        .json()
        .await
        .map_err(|e| format!("解析 Release 失败: {}", e))?;
    let tag = rel.get("tag_name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if tag.is_empty() {
        return Ok(None);
    }
    if let (Some(cur), Some(new)) = (parse_version(env!("CARGO_PKG_VERSION")), parse_version(&tag)) {
        if version_ge(&cur, &new) {
            return Ok(None);
        }
    }
    Ok(Some(tag))
}

/// 获取更新内容。Ok(None) = 已是最新版本。
async fn fetch_payload() -> Result<Option<(Vec<u8>, String)>, String> {
    // 1. 显式更新源：始终安装指向的内容
    if let Ok(u) = std::env::var("NGW_UPDATE_URL") {
        if !u.trim().is_empty() {
            let data = http_get_bytes(u.trim()).await?;
            return Ok(Some((data, "更新完成".into())));
        }
    }
    // 2. GitHub Releases 最新正式版
    let api_base = std::env::var("NGW_UPDATE_API_BASE")
        .unwrap_or_else(|_| "https://api.github.com".into())
        .trim_end_matches('/')
        .to_string();
    let repo = std::env::var("NGW_UPDATE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let api = format!("{}/repos/{}/releases/latest", api_base, repo);
    let client = http_client();
    let rel: Value = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("查询 Release 失败: {}", e))?
        .error_for_status()
        .map_err(|e| {
            if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                format!("仓库 {} 还没有正式 Release", repo)
            } else {
                format!("查询 Release 失败: {}", e)
            }
        })?
        .json()
        .await
        .map_err(|e| format!("解析 Release 失败: {}", e))?;

    let tag = rel.get("tag_name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if tag.is_empty() {
        return Err("更新源没有可用的正式 Release".into());
    }
    // 版本比较：Release 不比当前版本新就不动（避免重复下载/重启）
    if let (Some(cur), Some(new)) = (parse_version(env!("CARGO_PKG_VERSION")), parse_version(&tag)) {
        if version_ge(&cur, &new) {
            return Ok(None);
        }
    }
    // 按平台挑资产：nim-gateway-{os}-{arch}.tar.gz
    let want = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let asset_url = rel
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|a| {
                let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let url = a.get("browser_download_url").and_then(|x| x.as_str()).unwrap_or("");
                if !name.is_empty() && name.contains(&want) && name.ends_with(".tar.gz") && !url.is_empty() {
                    Some(url.to_string())
                } else {
                    None
                }
            })
        });
    let Some(asset_url) = asset_url else {
        return Err(format!("Release {} 缺少 {} 平台的更新包", tag, want));
    };
    let data = http_get_bytes(&asset_url).await?;
    Ok(Some((data, format!("已更新到 {}", tag))))
}

/// 在解包目录里找当前平台的二进制候选。
fn find_platform_binary(src: &Path) -> Option<PathBuf> {
    let exe_ext = if cfg!(windows) { ".exe" } else { "" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    fn walk(dir: &Path, skip: &[&str], out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            if p.is_dir() {
                if skip.contains(&name.as_str()) {
                    continue;
                }
                walk(&p, skip, out, depth + 1);
            } else if name.starts_with("nim-gateway") {
                out.push(p);
            }
        }
    }
    walk(src, SKIP_DIRS, &mut candidates, 0);
    candidates
        .into_iter()
        .find(|p| p.to_string_lossy().ends_with(exe_ext) || exe_ext.is_empty())
}

/// 备份当前代码与数据到 backup/（与 Python 版语义一致：回滚只能一次，用完即删）。
fn backup_current() -> Result<(), String> {
    let base = base_dir();
    let backup = base.join("backup");
    std::fs::create_dir_all(&backup).map_err(|e| format!("创建备份目录失败: {}", e))?;
    if let Some(exe) = exe_path() {
        let dst = backup.join(if cfg!(windows) { "nim-gateway.exe.bak" } else { "nim-gateway.bak" });
        match std::fs::copy(&exe, &dst) {
            Ok(_) => {}
            // 启动后文件被替换/删除（面板重新部署过）时源文件可能已不存在：
            // 不能因此把整个更新卡死——数据备份照做，只是本次没有二进制可回滚
            Err(e) => eprintln!("[update] 备份二进制失败({}: {})，本次无二进制回滚", exe.display(), e),
        }
    }
    // 数据备份：分表目录整目录拷贝（保留旧版单文件兼容）
    let db_dir = crate::store::db_dir();
    if db_dir.is_dir() {
        let dst = backup.join("data").join("db");
        copy_dir(&db_dir, &dst)?;
    }
    let legacy = crate::store::data_dir().join("db.json");
    if legacy.exists() {
        let data_dir = backup.join("data");
        std::fs::create_dir_all(&data_dir).ok();
        std::fs::copy(&legacy, data_dir.join("db.json")).map_err(|e| format!("备份数据失败: {}", e))?;
    }
    Ok(())
}

fn base_dir() -> PathBuf {
    std::env::var("NGW_BASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            exe_path()
                .and_then(|p| p.parent().map(|x| x.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        })
}

/// 拉取更新源 → 解包 → 校验平台二进制 → 备份 → 覆盖。任何一步失败都不动现有文件。
pub async fn remote_update() -> (bool, String) {
    if UPDATE_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return (false, "已有一次更新正在进行中，请稍后再试".into());
    }
    let r = remote_update_inner().await;
    UPDATE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    r
}

async fn remote_update_inner() -> (bool, String) {
    let (data, label) = match fetch_payload().await {
        Err(msg) => return (false, msg),
        Ok(None) => {
            return (true, format!("已是最新版本 v{}", env!("CARGO_PKG_VERSION")));
        }
        Ok(Some(x)) => x,
    };
    // 下载内容直接是二进制（ELF/PE 魔数）时跳过解包：自建更新源放一个裸二进制即可
    let direct_binary = data.starts_with(b"\x7fELF") || data.starts_with(b"MZ");
    let new_bin;
    let tmp = base_dir().join("_update_tmp");
    if direct_binary {
        let stage = tmp.join(if cfg!(windows) { "nim-gateway-update.exe" } else { "nim-gateway-update" });
        let _ = std::fs::remove_dir_all(&tmp);
        if std::fs::create_dir_all(&tmp).is_err() {
            return (false, "创建临时目录失败".into());
        }
        if std::fs::write(&stage, &data).is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
            return (false, "暂存更新二进制失败".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755));
        }
        new_bin = stage;
    } else {
        let _ = std::fs::remove_dir_all(&tmp);
        if std::fs::create_dir_all(&tmp).is_err() {
            return (false, "创建临时目录失败".into());
        }
        // tar.gz 解包（挡绝对路径与 ..）
        if let Err(e) = unpack_tar_gz(&data, &tmp) {
            let _ = std::fs::remove_dir_all(&tmp);
            return (false, format!("解包失败: {}", e));
        }
        match find_platform_binary(&tmp) {
            Some(b) => new_bin = b,
            None => {
                let _ = std::fs::remove_dir_all(&tmp);
                return (false, "更新包缺少当前平台的二进制，更新源需提供 Rust 版二进制".into());
            }
        }
    }
    // 演练模式（NGW_UPDATE_DRYRUN=1）：下载→解包→自检，不覆盖、不重启
    if std::env::var("NGW_UPDATE_DRYRUN").as_deref() == Ok("1") {
        let _ = std::fs::remove_dir_all(&tmp);
        return (true, "演练通过，更新包完整，未覆盖文件".into());
    }
    let Some(exe) = exe_path() else {
        let _ = std::fs::remove_dir_all(&tmp);
        return (false, "无法定位当前可执行文件".into());
    };
    if let Err(e) = backup_current() {
        let _ = std::fs::remove_dir_all(&tmp);
        return (false, e);
    }
    // 替换：Windows 上运行中的 exe 不能直接覆盖，先改名再写入
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&exe, &old).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return (false, "无法重命名当前可执行文件（可能被占用）".into());
    }
    if let Err(e) = std::fs::copy(&new_bin, &exe) {
        // 回滚改名，保证当前进程映像路径仍然有效
        let _ = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_dir_all(&tmp);
        return (false, format!("覆盖失败: {}", e));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // 先返回结果、随后自重启（与 Python 版的 2 秒延迟语义一致：由调用方先回响应）
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        crate::self_restart("远程更新完成");
    });
    (true, format!("{},网关正在自动重启", label))
}

/// 回滚到上次更新前（可执行文件 + 数据）。只能回滚一次，备份用完即删。
pub async fn remote_rollback() -> (bool, String) {
    if UPDATE_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return (false, "已有一次更新/回滚正在进行中，请稍后再试".into());
    }
    let r = remote_rollback_inner().await;
    UPDATE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    r
}

async fn remote_rollback_inner() -> (bool, String) {
    let backup = base_dir().join("backup");
    let bak_exe = backup.join(if cfg!(windows) { "nim-gateway.exe.bak" } else { "nim-gateway.bak" });
    if !bak_exe.exists() {
        return (false, "没有可用备份".into());
    }
    let Some(exe) = exe_path() else {
        return (false, "无法定位当前可执行文件".into());
    };
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&exe, &old).is_err() {
        return (false, "无法重命名当前可执行文件（可能被占用）".into());
    }
    if let Err(e) = std::fs::copy(&bak_exe, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return (false, format!("回滚失败: {}", e));
    }
    let _ = std::fs::remove_file(&bak_exe);
    // 数据回滚：分表目录整目录替换（回滚前当前数据先挪到 db.rollback 留底）
    let bak_dir = backup.join("data").join("db");
    if bak_dir.is_dir() {
        let cur = crate::store::db_dir();
        let rollback_copy = crate::store::data_dir().join("db.rollback");
        let _ = std::fs::remove_dir_all(&rollback_copy);
        if cur.is_dir() {
            let _ = std::fs::rename(&cur, &rollback_copy);
        }
        if copy_dir(&bak_dir, &cur).is_err() && rollback_copy.is_dir() {
            let _ = std::fs::remove_dir_all(&cur);
            let _ = std::fs::rename(&rollback_copy, &cur);
        }
        let _ = std::fs::remove_dir_all(&rollback_copy);
    }
    let bak_legacy = backup.join("data").join("db.json");
    if bak_legacy.exists() {
        let legacy = crate::store::data_dir().join("db.json");
        let _ = std::fs::copy(&bak_legacy, &legacy);
        let _ = std::fs::remove_file(&bak_legacy);
    }
    let _ = std::fs::remove_dir_all(&backup);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        crate::self_restart("回滚完成");
    });
    (true, "已回滚,网关正在自动重启(数秒)".into())
}

fn unpack_tar_gz(data: &[u8], dest: &Path) -> Result<(), String> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(data);
    let mut tar = tar::Archive::new(gz);
    let entries = tar.entries().map_err(|e| e.to_string())?;
    for e in entries {
        let mut e = e.map_err(|e| e.to_string())?;
        let path = e.path().map_err(|e| e.to_string())?.to_path_buf();
        let name = path.to_string_lossy().replace('\\', "/");
        if name.starts_with('/') || name.contains("../") {
            continue;
        }
        let t = e.header().entry_type();
        if t.is_dir() {
            let _ = std::fs::create_dir_all(dest.join(&path));
            continue;
        }
        if !t.is_file() {
            continue;
        }
        let out_path = dest.join(&path);
        // 最终防线：join 之后必须仍在解包目录内（Windows 盘符类绝对路径
        // 如 "C:/evil" 不以 / 开头，前面的字符串检查拦不住）
        if !out_path.starts_with(dest) {
            continue;
        }
        if let Some(parent) = out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        std::fs::write(&out_path, buf).map_err(|e| e.to_string())?;
        // tarball 里的执行位必须保留（Linux 上 fs::write 默认 0644，新二进制会起不来）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = e.header().mode().map(|m| m & 0o777).unwrap_or(0o644);
            let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

#[allow(unused)]
fn touch() {
    let _ = store();
    let _ = util::now_i();
}


fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败: {}", e))?;
    let rd = std::fs::read_dir(src).map_err(|e| format!("读取目录失败: {}", e))?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() {
            let name = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            if name.ends_with(".tmp") || name.contains(".corrupt-") || name.contains(".rollback") {
                continue;
            }
            std::fs::copy(&p, dst.join(&name)).map_err(|err| format!("拷贝 {} 失败: {}", name, err))?;
        }
    }
    Ok(())
}
